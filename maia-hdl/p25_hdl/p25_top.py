#
# Fishball P25 - Top-level IP core
#
# Demodulator design notes
# ------------------------
# P25 Phase 1 has TWO physical layer modulations:
#
#   * C4FM (Continuous 4-level Frequency Modulation) - pure 4FSK with
#     deviations of +-1800/+-600 Hz. Data is in instantaneous frequency.
#     Used by most non-simulcast P25 systems.
#   * LSM (Linear Simulcast Modulation) - actually a CQPSK with shaped
#     pulses. Used by simulcast systems where multiple co-located
#     transmitters radiate the same signal -- LSM's pulse shape gives
#     cleaner overlap than C4FM. Data is in carrier phase, not frequency.
#
# CURRENT STATUS (Phase 10.8, 2026-04-22): The C4FM HDL chain was
# retired in Phase 10.8 after extended FP&L testing showed the LSM
# chain decodes BOTH modulations cleanly on-air -- the LSM front-end's
# matched filter + Costas PLL + diff slicer copes with C4FM just fine,
# and we don't need a separate C4FM pipeline to keep around. Both the
# control channel AND the traffic channel now run a single LSM demod
# chain:
#
#   Control side:
#     ddc -> iq_packer ------------------------------> iq_dma             (0x1900_0000)
#         \-> lsm chain -> lsm_dibit_packer         -> lsm_dibit_dma      (0x1A00_0000)
#                      \-> pre_diff_iq_packer      -> pre_diff_iq_dma    (0x1F00_0000)
#
#   Traffic side:
#     traffic_ddc -> traffic_iq_packer --------------------> traffic_iq_dma             (0x1C00_0000)
#                 \-> traffic_lsm chain -> traffic_lsm_dibit_packer -> traffic_lsm_dibit_dma    (0x1B00_0000)
#                                       \-> traffic_pre_diff_iq_packer -> traffic_pre_diff_iq_dma (0x2000_0000)
#
# Recovered NIDs from each LSM chain are surfaced via separate AXI
# register banks: `lsm` at 0xA0 (control side, bank 5) and
# `traffic_lsm` at 0xC0 (traffic side, bank 6).
#
# LSM chain pipeline:
#
#   DDC out (62.5 kSPS)
#       |
#       v
#   LsmDecimator2  (/2)              -> 31.25 kSPS
#       |
#       v
#   LsmFir(LPF_TAPS_31250)           -- 83-tap baseband LPF
#       |
#       v
#   LsmFir(RRC_TAPS_31250)           -- 105-tap matched filter (alpha=0.2)
#       |
#       v
#   LsmDemod                          -- timing recovery + Costas PLL +
#       |                                pre-diff rotate + interleave +
#       |                                diff slicer + sync detect + BCH
#       |
#       +--> dibit_out / symbol_strobe    -> lsm_dibit_packer -> lsm_dibit_dma
#       +--> i_pre_diff_out / q_pre_diff_out / pre_diff_strobe_out
#       |                                 -> pre_diff_iq_packer -> pre_diff_iq_dma
#       +--> nid_event_strobe + (NAC, DUID, n_errors, ...)
#                                          -> `lsm` register bank
#
# Retired in Phase 10.8 (see doc/changes/ for the full list):
#   * C4FMDemod / SymbolTimingRecovery / DibitPacker on control chain
#     (and the traffic-side C4FM twins) -- LSM covers both modulations.
#   * dibit_dma / traffic_dma rings + their banks -- superseded by LSM.
#   * lsm_iq_dma / traffic_lsm_iq_dma (post-RRC matched-filter IQ
#     rings) + post_pll_iq_dma / traffic_post_pll_iq_dma (mid+sym
#     interleaved post-PLL rings) -- all superseded by the single
#     pre_diff_iq_dma / traffic_pre_diff_iq_dma pair added here
#     (carrier-derotated, AGC-scaled, pre-diff-slicer -- clean eye +
#     constellation with no PS-side rotate).
#
# SPDX-License-Identifier: MIT
#

import argparse
import sys
import os

from amaranth import *
from amaranth.lib.cdc import FFSynchronizer, PulseSynchronizer
import amaranth.back.verilog

from maia_hdl.axi4_lite import Axi4LiteRegisterBridge
from maia_hdl.cdc import RegisterCDC, RxIQCDC
from maia_hdl.clknx import ClkNxCommonEdge
from maia_hdl.dma import DmaStreamRingWrite
from maia_hdl.pluto_platform import PlutoPlatform
from maia_hdl.register import Access, Field, Registers, Register, RegisterMap
from maia_hdl.spectrometer import Spectrometer

from .p25ddc import P25DDC
from .iq_packer import IQPacker
from .lsm_decimator import LsmDecimator2
from .lsm_fir import LsmFir, LPF_TAPS_31250, RRC_TAPS_31250
from .lsm_demod import LsmDemod
from .config import P25Config
from . import configs

# IP core version
_version = '0.1.0'


class P25Core(Elaboratable):
    """Fishball P25 top-level IP core

    FPGA DSP pipeline for P25 Phase 1 trunking radio:
      AD9361 IQ -> DDC (tune + decimate) -> LSM demod chain ->
      dibit DMA + pre-diff IQ DMA + post-DDC IQ DMA to PS.

    Reuses Maia SDR DDC, register infrastructure, and DMA modules.
    """
    def __init__(self, config=P25Config()):
        config.validate()
        self.config = config
        # 8-bit AXI address = 256 bytes. Bank decoder below uses bits
        # [6:3] (4 bits = 16 banks of 32 B each). Highest bank used
        # after Phase 10.8 is 14 (traffic_pre_diff_iq at 0x1C0).
        self.axi4_awidth = 8
        self.s_axi_lite = ClockDomain()
        self.sampling = ClockDomain()
        self.sync = ClockDomain()
        self.clk3x = ClockDomain()
        # Phase 10.7: 2x domain for the wideband spectrometer's
        # Blackman-Harris window and FFT twiddle pipeline. Driven by
        # a new PS7 FCLK set to 2 × sync frequency (125 MHz).
        self.clk2x = ClockDomain()

        self.axi4lite = Axi4LiteRegisterBridge(
            self.axi4_awidth, name='s_axi_lite')

        # ── Control registers (0x00) ────────────────────────────────────
        self.control_registers = Registers(
            'control',
            {
                0b00: Register(
                    'product_id', [
                        # "p25f" = 0x70323566
                        Field('product_id', Access.R, 32, 0x70323566)
                    ]),
                0b01: Register('version', [
                    Field('bugfix', Access.R, 8,
                          int(_version.split('.')[2])),
                    Field('minor', Access.R, 8,
                          int(_version.split('.')[1])),
                    Field('major', Access.R, 8,
                          int(_version.split('.')[0])),
                    Field('platform', Access.R, 8, config.platform),
                ]),
                0b10: Register('control', [
                    Field('sdr_reset', Access.RW, 1, 1),
                ]),
                0b11: Register('interrupts', [
                    # Phase 6C: control-channel post-DDC IQ ring DMA
                    Field('iq_dma', Access.Rsticky, 1, 0),
                    # Phase 6E.9: control-channel LSM dibit ring DMA
                    Field('lsm_dibit_dma', Access.Rsticky, 1, 0),
                    # Phase 7A.2: traffic-channel LSM dibit ring DMA
                    Field('traffic_lsm_dibit_dma', Access.Rsticky, 1, 0),
                    # 2026-04-16: traffic-channel post-DDC IQ ring DMA
                    Field('traffic_iq_dma', Access.Rsticky, 1, 0),
                    # Phase 10.8 2026-04-22: pre-diff IQ ring DMAs
                    # (control + traffic). Tapped inside LsmDemodLoop
                    # after PLL rotate + mid/sym interleave, before
                    # the differential slicer. Feed the Plots tab
                    # (eye + constellation + deviation).
                    Field('pre_diff_iq_dma', Access.Rsticky, 1, 0),
                    Field('traffic_pre_diff_iq_dma', Access.Rsticky, 1, 0),
                    # Phase 10.7: wideband spectrometer.
                    Field('wideband_spec_dma', Access.Rsticky, 1, 0),
                ], interrupt=True),
            },
            2)

        # ── Control DDC registers (0x08) ──────────────────────────────
        # P25DDC: SDRTrunk-faithful v2 fork of maia_hdl.ddc.DDC with
        # unit-DC-gain coefficient convention and tightened stage 3
        # filter to fix the LsmDecimator2 fold-back bug. See
        # p25_hdl/p25ddc.py and doc/changes/041_p25ddc_fork.md.
        self.ddc = P25DDC('clk3x')

        self.sdr_registers = Registers(
            'sdr', {
                0b000: Register(
                    'ddc_coeff_addr', [
                        Field('coeff_waddr', Access.RW, 10, 0),
                    ]),
                0b010: Register(
                    'ddc_coeff', [
                        Field('coeff_wren', Access.Wpulse, 1, 0),
                        Field('coeff_wdata', Access.RW, 18, 0),
                    ]),
                0b011: Register(
                    'ddc_decimation', [
                        Field('decimation1', Access.RW, 7, 0),
                        Field('decimation2', Access.RW, 6, 0),
                        Field('decimation3', Access.RW, 7, 0),
                    ]),
                0b100: Register(
                    'ddc_frequency', [
                        Field('frequency', Access.RW, 28, 0),
                    ]),
                0b101: Register(
                    'ddc_control', [
                        Field('operations_minus_one1', Access.RW, 7, 0),
                        Field('operations_minus_one2', Access.RW, 6, 0),
                        Field('operations_minus_one3', Access.RW, 7, 0),
                        Field('odd_operations1', Access.RW, 1, 0),
                        Field('odd_operations3', Access.RW, 1, 0),
                        Field('bypass2', Access.RW, 1, 0),
                        Field('bypass3', Access.RW, 1, 0),
                        Field('enable_input', Access.RW, 1, 0),
                    ]),
            }, 3)

        # ── Control-channel post-DDC IQ ring DMA (Phase 6C) ───────────
        # Third tap of the control DDC output. The packer buffers two
        # consecutive (re, im) pairs into a 64-bit DMA word; see
        # iq_packer.py for the bit layout.
        #
        # Address: 0x1900_0000 / 256 KB ring (8 x 32 KB sub-buffers).
        self.iq_packer = IQPacker()
        self.iq_dma = DmaStreamRingWrite(
            config.iq_dma_address,
            config.iq_dma_num_buffers_log2,
            config.iq_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_iq')

        # ── Control channel IQ DMA registers (0x80, bank 4) ───────────
        self.iq_registers = Registers(
            'iq', {
                0b00: Register('iq_dma_status', [
                    Field('iq_overflow', Access.Rsticky, 1, 0),
                    Field('last_buffer', Access.R,
                          config.iq_dma_num_buffers_log2, -1),
                ]),
                0b01: Register('iq_dma_control', [
                    Field('iq_enable', Access.RW, 1, 0),
                ]),
                0b10: Register('iq_next_address', [
                    Field('next_address', Access.R, 32, 0),
                ]),
            },
            2)

        # ── Control channel LSM demod chain (Phase 6E.9) ──────────────
        # Single control-channel demod chain (replaces the retired
        # C4FM path). Consumes the control DDC output (62.5 kSPS,
        # 16-bit signed I+Q), decimates by 2 down to 31.25 kSPS,
        # filters with the 83-tap baseband LPF and the 105-tap RRC,
        # then runs LsmDemod (timing recovery + Costas PLL rotate +
        # mid/sym interleave + diff demod + slicer + sync detect +
        # BCH FEC). The recovered dibits exit via lsm_dibit_dma; the
        # recovered NIDs are surfaced via the `lsm` register bank.
        #
        # All four blocks live at top level (not wrapped) so each
        # block stays visible in the Vivado hierarchy + amaranth-sim
        # waveforms during bring-up.
        self.lsm_decimator = LsmDecimator2(width=16)
        self.lsm_lpf = LsmFir(LPF_TAPS_31250)
        self.lsm_rrc = LsmFir(RRC_TAPS_31250)
        self.lsm_demod = LsmDemod()
        # DibitPacker is bit-identical to the IQPacker pattern on the
        # wire side -- we just reuse it under a different name for
        # the LSM chain's dibit output so the PS ring DMA word format
        # matches the historical layout.
        from .dibit_packer import DibitPacker
        self.lsm_dibit_packer = DibitPacker()
        self.lsm_dibit_dma = DmaStreamRingWrite(
            config.lsm_dibit_dma_address,
            config.lsm_dibit_dma_num_buffers_log2,
            config.lsm_dibit_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_lsm_dibit')

        # ── Control channel pre-diff IQ ring DMA (Phase 10.8) ─────────
        # Tap inside LsmDemodLoop after LsmPllRotate but BEFORE the
        # differential slicer. Feeds `pre_diff_iq_dma` at
        # 0x1F00_0000. Packing + register layout mirror `iq_dma`.
        # Samples are carrier-derotated + AGC-scaled + interleaved
        # (mid, sym, mid, sym, ...) on adjacent strobe cycles, so the
        # PS renders a clean eye + constellation without PS-side
        # signal processing.
        self.pre_diff_iq_packer = IQPacker()
        self.pre_diff_iq_dma = DmaStreamRingWrite(
            config.pre_diff_iq_dma_address,
            config.pre_diff_iq_dma_num_buffers_log2,
            config.pre_diff_iq_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_pre_diff_iq')
        self.pre_diff_iq_registers = Registers(
            'pre_diff_iq', {
                0b00: Register('pre_diff_iq_dma_status', [
                    Field('pre_diff_iq_overflow', Access.Rsticky, 1, 0),
                    Field('last_buffer', Access.R,
                          config.pre_diff_iq_dma_num_buffers_log2, -1),
                ]),
                0b01: Register('pre_diff_iq_dma_control', [
                    Field('pre_diff_iq_enable', Access.RW, 1, 0),
                ]),
                0b10: Register('pre_diff_iq_next_address', [
                    Field('next_address', Access.R, 32, 0),
                ]),
            },
            2)

        # ── LSM register bank (0xA0, bank 5) ──────────────────────────
        # See doc/P25_ADDRESS_MAP.md for the canonical layout, including
        # the read/write semantics of every field and the PS-side polling
        # protocol for NID events.
        #
        # Bit-layout shorthand (bit positions inside each 32-bit word):
        #   lsm_control [0]   lsm_enable
        #               [1]   lsm_dibit_dma_enable
        #               [2]   lsm_dc_block_enable
        #   lsm_status  [0]   bch_busy
        #               [1]   in_nid_window
        #               [2]   nid_event             (Rsticky)
        #               [3]   nid_valid
        #               [10:4]  n_errors            (7 bits)
        #               [17:11] sync_distance       (7 bits)
        #               [18]  lsm_dibit_overflow    (Rsticky)
        #   lsm_nid     [11:0]  nac                 (12 bits)
        #               [15:12] duid                (4 bits)
        #   lsm_drop_count
        #               [15:0]  drop_count          (16 bits)
        #               [18:16] lsm_dibit_last_buffer (3 bits, init -1)
        #   lsm_dibit_next [31:0] next_address       (32 bits)
        #   lsm_debug   [15:0]  pll_dbg             (signed Q2.13)
        #               [31:16] sample_point_dbg    (signed Q4.10,
        #                                            top 16 bits of
        #                                            the 18-bit Q4.12)
        self.lsm_registers = Registers(
            'lsm', {
                0b000: Register('lsm_control', [
                    Field('lsm_enable', Access.RW, 1, 0),
                    Field('lsm_dibit_dma_enable', Access.RW, 1, 0),
                    # Phase 8A: W1P runtime-reset strobe. Writing 1
                    # drives a 1-cycle `reset_in` pulse into the
                    # LsmDemod chain.
                    Field('lsm_reset', Access.Wpulse, 1, 0),
                    # Phase 6G.1: front-end DC blocker enable.
                    Field('lsm_dc_block_enable', Access.RW, 1, 0),
                    # Phase 10-prep: per-symbol AGC enable.
                    Field('lsm_agc_enable', Access.RW, 1, 0),
                ]),
                0b001: Register('lsm_status', [
                    Field('bch_busy', Access.R, 1, 0),
                    Field('in_nid_window', Access.R, 1, 0),
                    Field('nid_event', Access.Rsticky, 1, 0),
                    Field('nid_valid', Access.R, 1, 0),
                    Field('n_errors', Access.R, 7, 0),
                    Field('sync_distance', Access.R, 7, 0),
                    Field('lsm_dibit_overflow', Access.Rsticky, 1, 0),
                ]),
                0b010: Register('lsm_nid', [
                    Field('nac', Access.R, 12, 0),
                    Field('duid', Access.R, 4, 0),
                ]),
                0b011: Register('lsm_drop_count', [
                    Field('drop_count', Access.R, 16, 0),
                    Field('lsm_dibit_last_buffer', Access.R,
                          config.lsm_dibit_dma_num_buffers_log2, -1),
                ]),
                0b100: Register('lsm_dibit_next', [
                    Field('next_address', Access.R, 32, 0),
                ]),
                0b101: Register('lsm_debug', [
                    Field('pll_dbg', Access.R, 16, 0),
                    Field('sample_point_dbg', Access.R, 16, 0),
                ]),
                # Phase 10-prep: per-symbol AGC debug taps.
                0b110: Register('lsm_agc_debug', [
                    Field('agc_gain_dbg', Access.R, 16, 0),
                    Field('agc_mag_dbg', Access.R, 16, 0),
                ]),
                # 2026-04-23: runtime AGC idle-gate threshold. Q1.15
                # raw. Default 256 = -42 dBFS relative to unit mag;
                # set to 0 to disable the gate (SDRTrunk-identical
                # behaviour). See lsm_agc.MAG_UPDATE_THRESHOLD_DEFAULT.
                0b111: Register('lsm_agc_config', [
                    Field('mag_update_threshold', Access.RW, 16, 256),
                ]),
            },
            3)

        # ── Traffic channel DDC ───────────────────────────────────────
        # Second P25DDC instance for the traffic channel (retuned by
        # PS on GroupVoiceChannelGrant). Same v2 filter convention as
        # the control DDC above; coefficient RAM is shared, so both
        # instances see the same (unit-DC-gain) filter tables.
        self.traffic_ddc = P25DDC('clk3x')

        # ── Traffic channel registers (0x60) ──────────────────────────
        # Phase 10.8: C4FM-related fields (traffic_demod_status /
        # traffic_demod_control / traffic_next_address) retired
        # alongside the C4FM chain. Only the DDC config registers
        # remain here.
        self.traffic_registers = Registers(
            'traffic', {
                0b000: Register(
                    'traffic_ddc_frequency', [
                        Field('frequency', Access.RW, 28, 0),
                    ]),
                0b001: Register(
                    'traffic_ddc_control', [
                        Field('operations_minus_one1', Access.RW, 7, 0),
                        Field('operations_minus_one2', Access.RW, 6, 0),
                        Field('operations_minus_one3', Access.RW, 7, 0),
                        Field('odd_operations1', Access.RW, 1, 0),
                        Field('odd_operations3', Access.RW, 1, 0),
                        Field('bypass2', Access.RW, 1, 0),
                        Field('bypass3', Access.RW, 1, 0),
                        Field('enable_input', Access.RW, 1, 0),
                    ]),
                0b010: Register(
                    'traffic_ddc_decimation', [
                        Field('decimation1', Access.RW, 7, 0),
                        Field('decimation2', Access.RW, 6, 0),
                        Field('decimation3', Access.RW, 7, 0),
                    ]),
            }, 3)

        # ── Traffic channel LSM demod chain (Phase 7A.2) ──────────────
        # Mirror of the control-side LSM chain. Both consume the same
        # `rxiq_cdc` output through independent DDC NCOs and share
        # coefficient RAM. Recovered dibits exit via
        # `traffic_lsm_dibit_dma`; recovered NIDs are surfaced via the
        # `traffic_lsm` register bank (bank 6 at offset 0xC0).
        self.traffic_lsm_decimator = LsmDecimator2(width=16)
        self.traffic_lsm_lpf = LsmFir(LPF_TAPS_31250)
        self.traffic_lsm_rrc = LsmFir(RRC_TAPS_31250)
        self.traffic_lsm_demod = LsmDemod()
        self.traffic_lsm_dibit_packer = DibitPacker()
        self.traffic_lsm_dibit_dma = DmaStreamRingWrite(
            config.traffic_lsm_dibit_dma_address,
            config.traffic_lsm_dibit_dma_num_buffers_log2,
            config.traffic_lsm_dibit_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_traffic_lsm_dibit')

        # ── Traffic LSM register bank (0xC0, bank 6) ──────────────────
        # Identical layout to the control-side `lsm` bank (0xA0).
        self.traffic_lsm_registers = Registers(
            'traffic_lsm', {
                0b000: Register('traffic_lsm_control', [
                    Field('traffic_lsm_enable', Access.RW, 1, 0),
                    Field('traffic_lsm_dibit_dma_enable', Access.RW, 1, 0),
                    Field('traffic_lsm_dc_block_enable', Access.RW, 1, 0),
                    Field('traffic_lsm_reset', Access.Wpulse, 1, 0),
                    Field('traffic_lsm_agc_enable', Access.RW, 1, 0),
                ]),
                0b001: Register('traffic_lsm_status', [
                    Field('bch_busy', Access.R, 1, 0),
                    Field('in_nid_window', Access.R, 1, 0),
                    Field('nid_event', Access.Rsticky, 1, 0),
                    Field('nid_valid', Access.R, 1, 0),
                    Field('n_errors', Access.R, 7, 0),
                    Field('sync_distance', Access.R, 7, 0),
                    Field('traffic_lsm_dibit_overflow', Access.Rsticky, 1, 0),
                ]),
                0b010: Register('traffic_lsm_nid', [
                    Field('nac', Access.R, 12, 0),
                    Field('duid', Access.R, 4, 0),
                ]),
                0b011: Register('traffic_lsm_drop_count', [
                    Field('drop_count', Access.R, 16, 0),
                    Field('traffic_lsm_dibit_last_buffer', Access.R,
                          config.traffic_lsm_dibit_dma_num_buffers_log2, -1),
                ]),
                0b100: Register('traffic_lsm_dibit_next', [
                    Field('next_address', Access.R, 32, 0),
                ]),
                0b101: Register('traffic_lsm_debug', [
                    Field('pll_dbg', Access.R, 16, 0),
                    Field('sample_point_dbg', Access.R, 16, 0),
                ]),
                0b110: Register('traffic_lsm_agc_debug', [
                    Field('agc_gain_dbg', Access.R, 16, 0),
                    Field('agc_mag_dbg', Access.R, 16, 0),
                ]),
                # 2026-04-23: mirror of lsm_agc_config for the traffic
                # chain. Independent so control/traffic gates can be
                # tuned separately (traffic may sit idle between
                # grants, where a different threshold is useful).
                0b111: Register('traffic_lsm_agc_config', [
                    Field('mag_update_threshold', Access.RW, 16, 256),
                ]),
            },
            3)

        # ── Traffic-channel post-DDC IQ ring DMA (2026-04-16) ─────────
        # Mirror of the control-side `iq_packer` + `iq_dma` (Phase 6C).
        # Address 0x1C00_0000, 256 KB ring.
        self.traffic_iq_packer = IQPacker()
        self.traffic_iq_dma = DmaStreamRingWrite(
            config.traffic_iq_dma_address,
            config.traffic_iq_dma_num_buffers_log2,
            config.traffic_iq_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_traffic_iq')

        # ── Traffic IQ DMA registers (0xE0, bank 7) ───────────────────
        self.traffic_iq_registers = Registers(
            'traffic_iq', {
                0b00: Register('traffic_iq_dma_status', [
                    Field('traffic_iq_overflow', Access.Rsticky, 1, 0),
                    Field('last_buffer', Access.R,
                          config.traffic_iq_dma_num_buffers_log2, -1),
                ]),
                0b01: Register('traffic_iq_dma_control', [
                    Field('traffic_iq_enable', Access.RW, 1, 0),
                ]),
                0b10: Register('traffic_iq_next_address', [
                    Field('next_address', Access.R, 32, 0),
                ]),
            },
            2)

        # ── Traffic channel pre-diff IQ ring DMA (Phase 10.8) ─────────
        # Traffic-side twin of `pre_diff_iq_dma`. Tap is
        # `traffic_lsm_demod.i_pre_diff_out / q_pre_diff_out /
        # pre_diff_strobe_out`. Register bank at 0x1C0 (bank 14).
        self.traffic_pre_diff_iq_packer = IQPacker()
        self.traffic_pre_diff_iq_dma = DmaStreamRingWrite(
            config.traffic_pre_diff_iq_dma_address,
            config.traffic_pre_diff_iq_dma_num_buffers_log2,
            config.traffic_pre_diff_iq_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_traffic_pre_diff_iq')
        self.traffic_pre_diff_iq_registers = Registers(
            'traffic_pre_diff_iq', {
                0b00: Register('traffic_pre_diff_iq_dma_status', [
                    Field('traffic_pre_diff_iq_overflow',
                          Access.Rsticky, 1, 0),
                    Field('last_buffer', Access.R,
                          config.traffic_pre_diff_iq_dma_num_buffers_log2, -1),
                ]),
                0b01: Register('traffic_pre_diff_iq_dma_control', [
                    Field('traffic_pre_diff_iq_enable', Access.RW, 1, 0),
                ]),
                0b10: Register('traffic_pre_diff_iq_next_address', [
                    Field('next_address', Access.R, 32, 0),
                ]),
            },
            2)

        # ── Wideband spectrometer (Phase 10.7) ────────────────────────
        # Directly instantiate the Maia SDR `Spectrometer` sub-module
        # (NOT the full `maia_sdr` IP wrapper, which would drag the
        # recorder + old DDC + its own register bank along for the
        # ride). Tap is pre-DDC off `rxiq_cdc` at the full AD9361
        # sample rate (preset-dependent, 2-16 MSPS). 4096-bin FFT + HW
        # integrator writes one 32 KB spectrum per completed
        # integration to `wideband_spec_dma`. Register bank at 0x180.
        self.wideband_spec = Spectrometer(
            config.wideband_spec_dma_address,
            config.wideband_spec_dma_num_buffers_log2,
            dma_name='m_axi_wideband_spec',
            domain_2x='clk2x', domain_3x='clk3x')
        self.wideband_spec_registers = Registers(
            'spectrometer', {
                0b00: Register('spec_control', [
                    Field('spec_enable', Access.RW, 1, 0),
                    Field('spec_peak_detect', Access.RW, 1, 0),
                    Field('spec_abort', Access.Wpulse, 1, 0),
                    Field('spec_num_integrations',
                          Access.RW, self.wideband_spec.nint_width, -1),
                ]),
                0b01: Register('spec_status', [
                    Field('spec_overflow', Access.Rsticky, 1, 0),
                    Field('spec_last_buffer', Access.R,
                          len(self.wideband_spec.last_buffer), -1),
                ]),
                0b10: Register('spec_next_address', [
                    Field('next_address', Access.R, 32, 0),
                ]),
            },
            2)

        # ── Register map ───────────────────────────────────────────────
        metadata = {
            'vendor': 'Andy Lee',
            'vendorID': 'fishball-p25',
            'name': 'Fishball P25',
            'series': 'Fishball P25',
            'version': _version,
            'description': f'Fishball P25 IP core (platform {config.platform})',
            'licenseText': 'SPDX-License-Identifier: MIT',
        }
        # Address banks: bits [6:3] of the word address select the bank
        # (4 bits = 16 banks max). Each bank spans 8 words = 32 bytes
        # (0x20). Phase 10.8 layout (after C4FM + superseded post-PLL /
        # post-LSM rings were retired):
        #
        #   0x00  bank 0   control       (product_id / version / ctrl / interrupts)
        #   0x20  bank 1   sdr           (control DDC config)
        #   0x60  bank 3   traffic       (traffic DDC config; C4FM fields dropped)
        #   0x80  bank 4   iq            (control post-DDC IQ DMA)
        #   0xA0  bank 5   lsm           (control LSM: dibit DMA + NID events)
        #   0xC0  bank 6   traffic_lsm   (traffic LSM: dibit DMA + NID events)
        #   0xE0  bank 7   traffic_iq    (traffic post-DDC IQ DMA)
        #   0x180 bank 12  spectrometer  (wideband FFT DMA)
        #   0x1A0 bank 13  pre_diff_iq          (control pre-diff IQ DMA)  -- new
        #   0x1C0 bank 14  traffic_pre_diff_iq  (traffic pre-diff IQ DMA)  -- new
        #
        # Banks 2, 8-11 are intentionally vacant (C4FM demod, post-LSM
        # matched-filter IQ, post-PLL IQ -- all retired).
        # See doc/P25_ADDRESS_MAP.md for the canonical bank table.
        self.register_map = RegisterMap({
            0x00:  self.control_registers,
            0x20:  self.sdr_registers,
            0x60:  self.traffic_registers,
            0x80:  self.iq_registers,                     # Phase 6C
            0xA0:  self.lsm_registers,                    # Phase 6E.9
            0xC0:  self.traffic_lsm_registers,            # Phase 7A.2
            0xE0:  self.traffic_iq_registers,             # 2026-04-16
            0x180: self.wideband_spec_registers,          # Phase 10.7
            0x1A0: self.pre_diff_iq_registers,            # Phase 10.8
            0x1C0: self.traffic_pre_diff_iq_registers,    # Phase 10.8
        }, metadata)

        # ── I/O signals ────────────────────────────────────────────────
        self.iq_in_width = 12
        self.re_in = Signal(self.iq_in_width)
        self.im_in = Signal(self.iq_in_width)
        self.interrupt_out = Signal()

    def ports(self):
        return (
            self.axi4lite.axi.ports()
            + self.traffic_lsm_dibit_dma.axi.ports()  # Phase 7A.2
            + self.iq_dma.axi.ports()       # Phase 6C
            + self.lsm_dibit_dma.axi.ports()  # Phase 6E.9
            + self.traffic_iq_dma.axi.ports()  # 2026-04-16
            + self.pre_diff_iq_dma.axi.ports()           # Phase 10.8
            + self.traffic_pre_diff_iq_dma.axi.ports()   # Phase 10.8
            + self.wideband_spec.dma.axi.ports()    # Phase 10.7
            + [
                self.re_in,
                self.im_in,
                self.interrupt_out,
                self.s_axi_lite.clk,
                self.s_axi_lite.rst,
                self.sampling.clk,
                self.sync.clk,
                self.sync.rst,
                self.clk3x.clk,
                self.clk2x.clk,
            ]
        )

    def svd(self):
        return self.register_map.svd()

    def elaborate(self, platform):
        m = Module()
        m.domains += [
            self.s_axi_lite,
            self.sampling,
            self.sync,
            self.clk3x,
            # Phase 10.7: 2x domain for the wideband spectrometer.
            self.clk2x,
        ]

        # Both LSM demod chains live in the global `sync` domain and
        # rely on (1) the explicit Phase 8A `reset_in` pulse fed from
        # the `lsm_reset` / `traffic_lsm_reset` Wpulse fields, plus
        # (2) strobe gating at the decimator (masked by
        # `lsm_enable` / `traffic_lsm_enable`) for per-call /
        # per-retune reset. No local per-chain clock domain.
        s_axi_lite_renamer = DomainRenamer({'sync': 's_axi_lite'})

        # ── Submodules ─────────────────────────────────────────────────
        m.submodules.axi4lite = s_axi_lite_renamer(self.axi4lite)
        m.submodules.control_registers = s_axi_lite_renamer(
            self.control_registers)
        m.submodules.ddc = self.ddc
        m.submodules.sdr_registers = self.sdr_registers
        m.submodules.sdr_registers_cdc = sdr_registers_cdc = RegisterCDC(
            's_axi_lite', 'sync', self.sdr_registers.aw)

        m.submodules.common_edge_3x = common_edge_3x = ClkNxCommonEdge(
            'sync', 'clk3x', 3)

        # ── RX IQ CDC (sampling -> sync) ──────────────────────────────
        m.submodules.rxiq_cdc = rxiq_cdc = RxIQCDC(
            'sampling', 'sync', self.iq_in_width)
        m.d.comb += [
            rxiq_cdc.re_in.eq(self.re_in),
            rxiq_cdc.im_in.eq(self.im_in),
        ]

        # ── Control DDC ──────────────────────────────────────────────
        m.d.comb += [
            self.ddc.common_edge.eq(common_edge_3x.common_edge),
            self.ddc.enable_input.eq(
                self.sdr_registers['ddc_control']['enable_input']),
            self.ddc.frequency.eq(
                self.sdr_registers['ddc_frequency']['frequency']),
            self.ddc.coeff_waddr.eq(
                self.sdr_registers['ddc_coeff_addr']['coeff_waddr']),
            self.ddc.coeff_wren.eq(
                self.sdr_registers['ddc_coeff']['coeff_wren']),
            self.ddc.coeff_wdata.eq(
                self.sdr_registers['ddc_coeff']['coeff_wdata']),
            self.ddc.decimation1.eq(
                self.sdr_registers['ddc_decimation']['decimation1']),
            self.ddc.decimation2.eq(
                self.sdr_registers['ddc_decimation']['decimation2']),
            self.ddc.decimation3.eq(
                self.sdr_registers['ddc_decimation']['decimation3']),
            self.ddc.bypass2.eq(
                self.sdr_registers['ddc_control']['bypass2']),
            self.ddc.bypass3.eq(
                self.sdr_registers['ddc_control']['bypass3']),
            self.ddc.operations_minus_one1.eq(
                self.sdr_registers['ddc_control']['operations_minus_one1']),
            self.ddc.operations_minus_one2.eq(
                self.sdr_registers['ddc_control']['operations_minus_one2']),
            self.ddc.operations_minus_one3.eq(
                self.sdr_registers['ddc_control']['operations_minus_one3']),
            self.ddc.odd_operations1.eq(
                self.sdr_registers['ddc_control']['odd_operations1']),
            self.ddc.odd_operations3.eq(
                self.sdr_registers['ddc_control']['odd_operations3']),
            self.ddc.strobe_in.eq(rxiq_cdc.strobe_out),
            self.ddc.re_in.eq(rxiq_cdc.re_out),
            self.ddc.im_in.eq(rxiq_cdc.im_out),
        ]

        # Phase 6C: control-channel post-DDC IQ ring DMA submodules.
        m.submodules.iq_packer = self.iq_packer
        m.submodules.iq_dma = self.iq_dma
        m.submodules.iq_registers = self.iq_registers
        m.submodules.iq_registers_cdc = iq_registers_cdc = RegisterCDC(
            's_axi_lite', 'sync', self.iq_registers.aw)

        # Phase 10.8: control-chain pre-diff IQ tap submodules.
        # Tapped from lsm_demod below.
        m.submodules.pre_diff_iq_packer = self.pre_diff_iq_packer
        m.submodules.pre_diff_iq_dma = self.pre_diff_iq_dma
        m.submodules.pre_diff_iq_registers = self.pre_diff_iq_registers
        m.submodules.pre_diff_iq_registers_cdc = (
            pre_diff_iq_registers_cdc) = RegisterCDC(
                's_axi_lite', 'sync', self.pre_diff_iq_registers.aw)

        # Phase 6E.9: control-channel LSM demod chain submodules.
        m.submodules.lsm_decimator = self.lsm_decimator
        m.submodules.lsm_lpf = self.lsm_lpf
        m.submodules.lsm_rrc = self.lsm_rrc
        m.submodules.lsm_demod = self.lsm_demod
        m.submodules.lsm_dibit_packer = self.lsm_dibit_packer
        m.submodules.lsm_dibit_dma = self.lsm_dibit_dma
        m.submodules.lsm_registers = self.lsm_registers
        m.submodules.lsm_registers_cdc = lsm_registers_cdc = RegisterCDC(
            's_axi_lite', 'sync', self.lsm_registers.aw)

        # DMA sub-buffer completion -> interrupt (sticky bit, cleared
        # by read). All DMA interrupts originate in the sync domain
        # (62.5 MHz clk_out1) but `control_registers` runs in
        # s_axi_lite (100 MHz clk_fpga_0). Use PulseSynchronizer per
        # pulse to guarantee exactly one destination-domain pulse per
        # source pulse regardless of clock relationship. See the
        # 2026-04-15 P25DDC v2 timing-closure CDC fix for the full
        # rationale.
        interrupts_reg = self.control_registers['interrupts']

        m.submodules.iq_dma_irq_sync = iq_dma_irq_sync = (
            PulseSynchronizer('sync', 's_axi_lite'))
        m.submodules.lsm_dibit_dma_irq_sync = lsm_dibit_dma_irq_sync = (
            PulseSynchronizer('sync', 's_axi_lite'))
        m.submodules.traffic_lsm_dibit_dma_irq_sync = (
            traffic_lsm_dibit_dma_irq_sync) = (
                PulseSynchronizer('sync', 's_axi_lite'))
        m.submodules.traffic_iq_dma_irq_sync = traffic_iq_dma_irq_sync = (
            PulseSynchronizer('sync', 's_axi_lite'))
        # Phase 10.8: pre-diff IQ DMA interrupts.
        m.submodules.pre_diff_iq_dma_irq_sync = (
            pre_diff_iq_dma_irq_sync) = (
                PulseSynchronizer('sync', 's_axi_lite'))
        m.submodules.traffic_pre_diff_iq_dma_irq_sync = (
            traffic_pre_diff_iq_dma_irq_sync) = (
                PulseSynchronizer('sync', 's_axi_lite'))
        # Phase 10.7: wideband spectrometer.
        m.submodules.wideband_spec_dma_irq_sync = (
            wideband_spec_dma_irq_sync) = (
                PulseSynchronizer('sync', 's_axi_lite'))

        m.d.comb += [
            iq_dma_irq_sync.i.eq(self.iq_dma.interrupt),
            lsm_dibit_dma_irq_sync.i.eq(self.lsm_dibit_dma.interrupt),
            traffic_lsm_dibit_dma_irq_sync.i.eq(
                self.traffic_lsm_dibit_dma.interrupt),
            traffic_iq_dma_irq_sync.i.eq(self.traffic_iq_dma.interrupt),
            pre_diff_iq_dma_irq_sync.i.eq(
                self.pre_diff_iq_dma.interrupt),
            traffic_pre_diff_iq_dma_irq_sync.i.eq(
                self.traffic_pre_diff_iq_dma.interrupt),
            wideband_spec_dma_irq_sync.i.eq(
                self.wideband_spec.interrupt_out),
            # Feed the synchronized pulses into the Rsticky bits.
            interrupts_reg['pre_diff_iq_dma'].eq(
                pre_diff_iq_dma_irq_sync.o),
            interrupts_reg['traffic_pre_diff_iq_dma'].eq(
                traffic_pre_diff_iq_dma_irq_sync.o),
            interrupts_reg['wideband_spec_dma'].eq(
                wideband_spec_dma_irq_sync.o),
        ]

        # ── Control-channel IQ ring DMA (Phase 6C) ────────────────────
        # Tap of the control DDC output. The packer fans the same
        # re_out / im_out / strobe_out that feed the LSM chain below
        # into a 64-bit DMA stream (two IQ pairs per word, sample 0 in
        # the low half). See iq_packer.py for the bit layout and
        # doc/P25_ADDRESS_MAP.md for the DDR carve-out.
        m.d.comb += [
            self.iq_packer.re_in.eq(self.ddc.re_out),
            self.iq_packer.im_in.eq(self.ddc.im_out),
            self.iq_packer.strobe_in.eq(self.ddc.strobe_out),
        ]

        # IQ packer -> ring DMA stream (handshake-driven backpressure)
        m.d.comb += [
            self.iq_dma.stream_data.eq(self.iq_packer.data_out),
            self.iq_dma.stream_valid.eq(self.iq_packer.data_valid),
            self.iq_packer.stream_ready.eq(self.iq_dma.stream_ready),
        ]

        # IQ DMA enable + interrupt + status registers
        m.d.comb += [
            self.iq_dma.enable.eq(
                self.iq_registers['iq_dma_control']['iq_enable']),
            interrupts_reg['iq_dma'].eq(iq_dma_irq_sync.o),
            self.iq_registers['iq_dma_status']['iq_overflow'].eq(
                self.iq_packer.overflow),
            self.iq_registers['iq_dma_status']['last_buffer'].eq(
                self.iq_dma.last_buffer),
            self.iq_registers['iq_next_address']['next_address'].eq(
                self.iq_dma.axi.awaddr),
        ]

        # ── Control-channel LSM demod chain (Phase 6E.9) ──────────────
        # Pipeline:
        #   DDC (62.5 kSPS) -> /2 decimator -> LPF -> RRC ->
        #     LsmDemod -> { dibit_packer -> lsm_dibit_dma,
        #                   pre_diff taps -> pre_diff_iq_packer -> pre_diff_iq_dma,
        #                   nid event registers }
        #
        # Master enable: lsm_control.lsm_enable gates the strobe at
        # the very front of LsmDecimator2, so when 0 every downstream
        # block sees no strobes and goes quiescent (no PLL drift, no
        # BCH sweeps, no spurious dibits).
        lsm_enable = self.lsm_registers['lsm_control']['lsm_enable']

        # Stage 1: control DDC -> LSM /2 decimator
        m.d.comb += [
            self.lsm_decimator.re_in.eq(self.ddc.re_out),
            self.lsm_decimator.im_in.eq(self.ddc.im_out),
            self.lsm_decimator.strobe_in.eq(
                self.ddc.strobe_out & lsm_enable),
        ]
        # Stage 2: decimator -> 83-tap LPF
        m.d.comb += [
            self.lsm_lpf.re_in.eq(self.lsm_decimator.re_out),
            self.lsm_lpf.im_in.eq(self.lsm_decimator.im_out),
            self.lsm_lpf.strobe_in.eq(self.lsm_decimator.strobe_out),
        ]
        # Stage 3: LPF -> 105-tap RRC matched filter
        m.d.comb += [
            self.lsm_rrc.re_in.eq(self.lsm_lpf.re_out),
            self.lsm_rrc.im_in.eq(self.lsm_lpf.im_out),
            self.lsm_rrc.strobe_in.eq(self.lsm_lpf.strobe_out),
        ]

        # Stage 4: RRC -> LsmDemod (timing recovery + diff demod +
        # PLL rotate + slicer + sync detect + BCH FEC). Outputs:
        # `dibit_out`/`symbol_strobe` (passthrough to dibit DMA),
        # `i_pre_diff_out`/`q_pre_diff_out`/`pre_diff_strobe_out`
        # (post-PLL, pre-slicer, interleaved -- feed the Phase 10.8
        # pre-diff IQ DMA), and `nid_event_strobe` + the NID fields
        # (latched into the lsm register bank below).
        m.d.comb += [
            self.lsm_demod.re_in.eq(self.lsm_rrc.re_out),
            self.lsm_demod.im_in.eq(self.lsm_rrc.im_out),
            self.lsm_demod.strobe_in.eq(self.lsm_rrc.strobe_out),
            self.lsm_demod.dc_block_enable.eq(
                self.lsm_registers['lsm_control']['lsm_dc_block_enable']),
            # Phase 10-prep: per-symbol AGC enable.
            self.lsm_demod.agc_enable.eq(
                self.lsm_registers['lsm_control']['lsm_agc_enable']),
            # Phase 8A: runtime reset strobe.
            self.lsm_demod.reset_in.eq(
                self.lsm_registers['lsm_control']['lsm_reset']),
        ]

        # Stage 5: LsmDemod dibits -> packer -> ring DMA stream.
        m.d.comb += [
            self.lsm_dibit_packer.dibit_in.eq(self.lsm_demod.dibit_out),
            self.lsm_dibit_packer.symbol_strobe.eq(
                self.lsm_demod.symbol_strobe),
            self.lsm_dibit_dma.stream_data.eq(
                self.lsm_dibit_packer.data_out),
            self.lsm_dibit_dma.stream_valid.eq(
                self.lsm_dibit_packer.data_valid),
            self.lsm_dibit_packer.stream_ready.eq(
                self.lsm_dibit_dma.stream_ready),
            self.lsm_dibit_dma.enable.eq(
                self.lsm_registers['lsm_control']['lsm_dibit_dma_enable']),
            interrupts_reg['lsm_dibit_dma'].eq(lsm_dibit_dma_irq_sync.o),
        ]

        # Stage 5b: Phase 10.8 pre-diff IQ tap. LsmDemod surfaces
        # carrier-derotated + AGC-scaled samples interleaved on
        # adjacent `pre_diff_strobe_out` cycles (mid, sym, mid, sym,
        # ...). Pack two samples per 64-bit word with the same
        # layout as `iq_dma` (sample 0 in low half).
        m.d.comb += [
            self.pre_diff_iq_packer.re_in.eq(self.lsm_demod.i_pre_diff_out),
            self.pre_diff_iq_packer.im_in.eq(self.lsm_demod.q_pre_diff_out),
            self.pre_diff_iq_packer.strobe_in.eq(
                self.lsm_demod.pre_diff_strobe_out),
            self.pre_diff_iq_dma.stream_data.eq(
                self.pre_diff_iq_packer.data_out),
            self.pre_diff_iq_dma.stream_valid.eq(
                self.pre_diff_iq_packer.data_valid),
            self.pre_diff_iq_packer.stream_ready.eq(
                self.pre_diff_iq_dma.stream_ready),
            self.pre_diff_iq_dma.enable.eq(
                self.pre_diff_iq_registers[
                    'pre_diff_iq_dma_control']['pre_diff_iq_enable']),
            self.pre_diff_iq_registers[
                'pre_diff_iq_dma_status']['pre_diff_iq_overflow'].eq(
                self.pre_diff_iq_packer.overflow),
            self.pre_diff_iq_registers[
                'pre_diff_iq_dma_status']['last_buffer'].eq(
                self.pre_diff_iq_dma.last_buffer),
            self.pre_diff_iq_registers[
                'pre_diff_iq_next_address']['next_address'].eq(
                self.pre_diff_iq_dma.axi.awaddr),
        ]

        # Stage 6: NID event latching.
        latched_nac = Signal(12, reset_less=True)
        latched_duid = Signal(4, reset_less=True)
        latched_n_errors = Signal(7, reset_less=True)
        latched_valid = Signal(reset_less=True)
        latched_sync_distance = Signal(7, reset_less=True)
        with m.If(self.lsm_demod.nid_event_strobe):
            m.d.sync += [
                latched_nac.eq(self.lsm_demod.nac_out),
                latched_duid.eq(self.lsm_demod.duid_out),
                latched_n_errors.eq(self.lsm_demod.n_errors_out),
                latched_valid.eq(self.lsm_demod.valid_out),
                latched_sync_distance.eq(self.lsm_demod.sync_distance_out),
            ]

        lsm_status = self.lsm_registers['lsm_status']
        lsm_nid = self.lsm_registers['lsm_nid']
        lsm_drop = self.lsm_registers['lsm_drop_count']
        lsm_debug = self.lsm_registers['lsm_debug']
        m.d.comb += [
            lsm_status['bch_busy'].eq(self.lsm_demod.bch_busy),
            lsm_status['in_nid_window'].eq(self.lsm_demod.in_nid_window),
            lsm_status['nid_event'].eq(self.lsm_demod.nid_event_strobe),
            lsm_status['nid_valid'].eq(latched_valid),
            lsm_status['n_errors'].eq(latched_n_errors),
            lsm_status['sync_distance'].eq(latched_sync_distance),
            lsm_status['lsm_dibit_overflow'].eq(
                self.lsm_dibit_packer.overflow),
            lsm_nid['nac'].eq(latched_nac),
            lsm_nid['duid'].eq(latched_duid),
            lsm_drop['drop_count'].eq(self.lsm_demod.nid_drop_count),
            lsm_drop['lsm_dibit_last_buffer'].eq(
                self.lsm_dibit_dma.last_buffer),
            self.lsm_registers['lsm_dibit_next']['next_address'].eq(
                self.lsm_dibit_dma.axi.awaddr),
            lsm_debug['pll_dbg'].eq(self.lsm_demod.pll_dbg),
            lsm_debug['sample_point_dbg'].eq(
                self.lsm_demod.sample_point_dbg[2:]),
        ]

        # Phase 10-prep: per-symbol AGC debug taps.
        lsm_agc_debug = self.lsm_registers['lsm_agc_debug']
        m.d.comb += [
            lsm_agc_debug['agc_gain_dbg'].eq(self.lsm_demod.agc_gain_dbg),
            lsm_agc_debug['agc_mag_dbg'].eq(self.lsm_demod.agc_mag_dbg),
        ]
        # 2026-04-23: runtime AGC idle-gate threshold (control chain).
        # Defaults via register reset to 256 (see lsm_agc_config init).
        lsm_agc_config = self.lsm_registers['lsm_agc_config']
        m.d.comb += [
            self.lsm_demod.agc_mag_update_threshold_in.eq(
                lsm_agc_config['mag_update_threshold']),
        ]

        # ── Traffic channel DDC ───────────────────────────────────────
        m.submodules.traffic_ddc = self.traffic_ddc
        m.submodules.traffic_registers = self.traffic_registers
        m.submodules.traffic_registers_cdc = traffic_registers_cdc = RegisterCDC(
            's_axi_lite', 'sync', self.traffic_registers.aw)

        # Traffic DDC shares the same IQ input as control DDC but has
        # its own NCO frequency for independent tuning.
        m.d.comb += [
            self.traffic_ddc.common_edge.eq(common_edge_3x.common_edge),
            self.traffic_ddc.enable_input.eq(
                self.traffic_registers['traffic_ddc_control']['enable_input']),
            self.traffic_ddc.frequency.eq(
                self.traffic_registers['traffic_ddc_frequency']['frequency']),
            # Share coefficients with control DDC (same P25 filter shape)
            self.traffic_ddc.coeff_waddr.eq(
                self.sdr_registers['ddc_coeff_addr']['coeff_waddr']),
            self.traffic_ddc.coeff_wren.eq(
                self.sdr_registers['ddc_coeff']['coeff_wren']),
            self.traffic_ddc.coeff_wdata.eq(
                self.sdr_registers['ddc_coeff']['coeff_wdata']),
            self.traffic_ddc.decimation1.eq(
                self.traffic_registers['traffic_ddc_decimation']['decimation1']),
            self.traffic_ddc.decimation2.eq(
                self.traffic_registers['traffic_ddc_decimation']['decimation2']),
            self.traffic_ddc.decimation3.eq(
                self.traffic_registers['traffic_ddc_decimation']['decimation3']),
            self.traffic_ddc.bypass2.eq(
                self.traffic_registers['traffic_ddc_control']['bypass2']),
            self.traffic_ddc.bypass3.eq(
                self.traffic_registers['traffic_ddc_control']['bypass3']),
            self.traffic_ddc.operations_minus_one1.eq(
                self.traffic_registers['traffic_ddc_control']['operations_minus_one1']),
            self.traffic_ddc.operations_minus_one2.eq(
                self.traffic_registers['traffic_ddc_control']['operations_minus_one2']),
            self.traffic_ddc.operations_minus_one3.eq(
                self.traffic_registers['traffic_ddc_control']['operations_minus_one3']),
            self.traffic_ddc.odd_operations1.eq(
                self.traffic_registers['traffic_ddc_control']['odd_operations1']),
            self.traffic_ddc.odd_operations3.eq(
                self.traffic_registers['traffic_ddc_control']['odd_operations3']),
            # Same IQ input as control DDC
            self.traffic_ddc.strobe_in.eq(rxiq_cdc.strobe_out),
            self.traffic_ddc.re_in.eq(rxiq_cdc.re_out),
            self.traffic_ddc.im_in.eq(rxiq_cdc.im_out),
        ]

        # ── Traffic-channel LSM demod chain (Phase 7A.2) ──────────────
        # Pipeline (mirror of the control-side LSM chain):
        #   traffic_ddc (62.5 kSPS) -> /2 decimator -> LPF -> RRC ->
        #     LsmDemod -> { dibit_packer -> traffic_lsm_dibit_dma,
        #                   pre_diff taps -> traffic_pre_diff_iq_packer
        #                                 -> traffic_pre_diff_iq_dma,
        #                   nid event registers (traffic_lsm bank) }
        m.submodules.traffic_lsm_decimator = self.traffic_lsm_decimator
        m.submodules.traffic_lsm_lpf = self.traffic_lsm_lpf
        m.submodules.traffic_lsm_rrc = self.traffic_lsm_rrc
        m.submodules.traffic_lsm_demod = self.traffic_lsm_demod
        m.submodules.traffic_lsm_dibit_packer = self.traffic_lsm_dibit_packer
        m.submodules.traffic_lsm_dibit_dma = self.traffic_lsm_dibit_dma
        m.submodules.traffic_lsm_registers = self.traffic_lsm_registers
        m.submodules.traffic_lsm_registers_cdc = traffic_lsm_registers_cdc = \
            RegisterCDC(
                's_axi_lite', 'sync', self.traffic_lsm_registers.aw)

        traffic_lsm_enable = self.traffic_lsm_registers[
            'traffic_lsm_control']['traffic_lsm_enable']

        # Stage 1: traffic DDC -> LSM /2 decimator
        m.d.comb += [
            self.traffic_lsm_decimator.re_in.eq(self.traffic_ddc.re_out),
            self.traffic_lsm_decimator.im_in.eq(self.traffic_ddc.im_out),
            self.traffic_lsm_decimator.strobe_in.eq(
                self.traffic_ddc.strobe_out & traffic_lsm_enable),
        ]
        # Stage 2: decimator -> 83-tap LPF
        m.d.comb += [
            self.traffic_lsm_lpf.re_in.eq(self.traffic_lsm_decimator.re_out),
            self.traffic_lsm_lpf.im_in.eq(self.traffic_lsm_decimator.im_out),
            self.traffic_lsm_lpf.strobe_in.eq(
                self.traffic_lsm_decimator.strobe_out),
        ]
        # Stage 3: LPF -> 105-tap RRC matched filter
        m.d.comb += [
            self.traffic_lsm_rrc.re_in.eq(self.traffic_lsm_lpf.re_out),
            self.traffic_lsm_rrc.im_in.eq(self.traffic_lsm_lpf.im_out),
            self.traffic_lsm_rrc.strobe_in.eq(
                self.traffic_lsm_lpf.strobe_out),
        ]
        # Stage 4: RRC -> LsmDemod (identical to control side).
        m.d.comb += [
            self.traffic_lsm_demod.re_in.eq(self.traffic_lsm_rrc.re_out),
            self.traffic_lsm_demod.im_in.eq(self.traffic_lsm_rrc.im_out),
            self.traffic_lsm_demod.strobe_in.eq(
                self.traffic_lsm_rrc.strobe_out),
            self.traffic_lsm_demod.dc_block_enable.eq(
                self.traffic_lsm_registers[
                    'traffic_lsm_control']['traffic_lsm_dc_block_enable']),
            self.traffic_lsm_demod.agc_enable.eq(
                self.traffic_lsm_registers[
                    'traffic_lsm_control']['traffic_lsm_agc_enable']),
            self.traffic_lsm_demod.reset_in.eq(
                self.traffic_lsm_registers[
                    'traffic_lsm_control']['traffic_lsm_reset']),
        ]

        # Stage 5: LsmDemod dibits -> packer -> ring DMA stream.
        m.d.comb += [
            self.traffic_lsm_dibit_packer.dibit_in.eq(
                self.traffic_lsm_demod.dibit_out),
            self.traffic_lsm_dibit_packer.symbol_strobe.eq(
                self.traffic_lsm_demod.symbol_strobe),
            self.traffic_lsm_dibit_dma.stream_data.eq(
                self.traffic_lsm_dibit_packer.data_out),
            self.traffic_lsm_dibit_dma.stream_valid.eq(
                self.traffic_lsm_dibit_packer.data_valid),
            self.traffic_lsm_dibit_packer.stream_ready.eq(
                self.traffic_lsm_dibit_dma.stream_ready),
            self.traffic_lsm_dibit_dma.enable.eq(
                self.traffic_lsm_registers[
                    'traffic_lsm_control']['traffic_lsm_dibit_dma_enable']),
            interrupts_reg['traffic_lsm_dibit_dma'].eq(
                traffic_lsm_dibit_dma_irq_sync.o),
        ]

        # Stage 6: NID event latching (mirror of control side).
        traffic_latched_nac = Signal(12, reset_less=True)
        traffic_latched_duid = Signal(4, reset_less=True)
        traffic_latched_n_errors = Signal(7, reset_less=True)
        traffic_latched_valid = Signal(reset_less=True)
        traffic_latched_sync_distance = Signal(7, reset_less=True)
        with m.If(self.traffic_lsm_demod.nid_event_strobe):
            m.d.sync += [
                traffic_latched_nac.eq(self.traffic_lsm_demod.nac_out),
                traffic_latched_duid.eq(self.traffic_lsm_demod.duid_out),
                traffic_latched_n_errors.eq(
                    self.traffic_lsm_demod.n_errors_out),
                traffic_latched_valid.eq(self.traffic_lsm_demod.valid_out),
                traffic_latched_sync_distance.eq(
                    self.traffic_lsm_demod.sync_distance_out),
            ]

        traffic_lsm_status = self.traffic_lsm_registers['traffic_lsm_status']
        traffic_lsm_nid = self.traffic_lsm_registers['traffic_lsm_nid']
        traffic_lsm_drop = self.traffic_lsm_registers['traffic_lsm_drop_count']
        traffic_lsm_debug = self.traffic_lsm_registers['traffic_lsm_debug']
        m.d.comb += [
            traffic_lsm_status['bch_busy'].eq(
                self.traffic_lsm_demod.bch_busy),
            traffic_lsm_status['in_nid_window'].eq(
                self.traffic_lsm_demod.in_nid_window),
            traffic_lsm_status['nid_event'].eq(
                self.traffic_lsm_demod.nid_event_strobe),
            traffic_lsm_status['nid_valid'].eq(traffic_latched_valid),
            traffic_lsm_status['n_errors'].eq(traffic_latched_n_errors),
            traffic_lsm_status['sync_distance'].eq(
                traffic_latched_sync_distance),
            traffic_lsm_status['traffic_lsm_dibit_overflow'].eq(
                self.traffic_lsm_dibit_packer.overflow),
            traffic_lsm_nid['nac'].eq(traffic_latched_nac),
            traffic_lsm_nid['duid'].eq(traffic_latched_duid),
            traffic_lsm_drop['drop_count'].eq(
                self.traffic_lsm_demod.nid_drop_count),
            traffic_lsm_drop['traffic_lsm_dibit_last_buffer'].eq(
                self.traffic_lsm_dibit_dma.last_buffer),
            self.traffic_lsm_registers[
                'traffic_lsm_dibit_next']['next_address'].eq(
                self.traffic_lsm_dibit_dma.axi.awaddr),
            traffic_lsm_debug['pll_dbg'].eq(self.traffic_lsm_demod.pll_dbg),
            traffic_lsm_debug['sample_point_dbg'].eq(
                self.traffic_lsm_demod.sample_point_dbg[2:]),
        ]

        # Phase 10-prep: traffic-side per-symbol AGC debug taps.
        traffic_lsm_agc_debug = self.traffic_lsm_registers[
            'traffic_lsm_agc_debug']
        m.d.comb += [
            traffic_lsm_agc_debug['agc_gain_dbg'].eq(
                self.traffic_lsm_demod.agc_gain_dbg),
            traffic_lsm_agc_debug['agc_mag_dbg'].eq(
                self.traffic_lsm_demod.agc_mag_dbg),
        ]
        # 2026-04-23: runtime AGC idle-gate threshold (traffic chain).
        traffic_lsm_agc_config = self.traffic_lsm_registers[
            'traffic_lsm_agc_config']
        m.d.comb += [
            self.traffic_lsm_demod.agc_mag_update_threshold_in.eq(
                traffic_lsm_agc_config['mag_update_threshold']),
        ]

        # ── Traffic-channel post-DDC IQ ring DMA (2026-04-16) ─────────
        # Mirrors the control-side `iq_dma` wiring (Phase 6C) for the
        # traffic chain. Taps the same `traffic_ddc.re_out/im_out/
        # strobe_out` that also feed the traffic LSM chain, packs two
        # consecutive IQ pairs into a 64-bit DMA word, and streams to
        # DDR at 0x1C00_0000.
        m.submodules.traffic_iq_packer = self.traffic_iq_packer
        m.submodules.traffic_iq_dma = self.traffic_iq_dma
        m.submodules.traffic_iq_registers = self.traffic_iq_registers
        m.submodules.traffic_iq_registers_cdc = traffic_iq_registers_cdc = \
            RegisterCDC(
                's_axi_lite', 'sync', self.traffic_iq_registers.aw)

        m.d.comb += [
            self.traffic_iq_packer.re_in.eq(self.traffic_ddc.re_out),
            self.traffic_iq_packer.im_in.eq(self.traffic_ddc.im_out),
            self.traffic_iq_packer.strobe_in.eq(self.traffic_ddc.strobe_out),
        ]

        # Packer -> ring DMA stream (handshake backpressure)
        m.d.comb += [
            self.traffic_iq_dma.stream_data.eq(self.traffic_iq_packer.data_out),
            self.traffic_iq_dma.stream_valid.eq(self.traffic_iq_packer.data_valid),
            self.traffic_iq_packer.stream_ready.eq(
                self.traffic_iq_dma.stream_ready),
        ]

        # DMA enable + interrupt + status registers
        m.d.comb += [
            self.traffic_iq_dma.enable.eq(
                self.traffic_iq_registers[
                    'traffic_iq_dma_control']['traffic_iq_enable']),
            interrupts_reg['traffic_iq_dma'].eq(traffic_iq_dma_irq_sync.o),
            self.traffic_iq_registers[
                'traffic_iq_dma_status']['traffic_iq_overflow'].eq(
                self.traffic_iq_packer.overflow),
            self.traffic_iq_registers[
                'traffic_iq_dma_status']['last_buffer'].eq(
                self.traffic_iq_dma.last_buffer),
            self.traffic_iq_registers[
                'traffic_iq_next_address']['next_address'].eq(
                self.traffic_iq_dma.axi.awaddr),
        ]

        # ── Traffic-channel pre-diff IQ ring DMA (Phase 10.8) ─────────
        # Traffic-side twin of `pre_diff_iq_dma`. Tap is
        # `traffic_lsm_demod.i_pre_diff_out / q_pre_diff_out /
        # pre_diff_strobe_out`.
        m.submodules.traffic_pre_diff_iq_packer = (
            self.traffic_pre_diff_iq_packer)
        m.submodules.traffic_pre_diff_iq_dma = self.traffic_pre_diff_iq_dma
        m.submodules.traffic_pre_diff_iq_registers = (
            self.traffic_pre_diff_iq_registers)
        m.submodules.traffic_pre_diff_iq_registers_cdc = (
            traffic_pre_diff_iq_registers_cdc) = RegisterCDC(
                's_axi_lite', 'sync',
                self.traffic_pre_diff_iq_registers.aw)

        m.d.comb += [
            self.traffic_pre_diff_iq_packer.re_in.eq(
                self.traffic_lsm_demod.i_pre_diff_out),
            self.traffic_pre_diff_iq_packer.im_in.eq(
                self.traffic_lsm_demod.q_pre_diff_out),
            self.traffic_pre_diff_iq_packer.strobe_in.eq(
                self.traffic_lsm_demod.pre_diff_strobe_out),
            self.traffic_pre_diff_iq_dma.stream_data.eq(
                self.traffic_pre_diff_iq_packer.data_out),
            self.traffic_pre_diff_iq_dma.stream_valid.eq(
                self.traffic_pre_diff_iq_packer.data_valid),
            self.traffic_pre_diff_iq_packer.stream_ready.eq(
                self.traffic_pre_diff_iq_dma.stream_ready),
            self.traffic_pre_diff_iq_dma.enable.eq(
                self.traffic_pre_diff_iq_registers[
                    'traffic_pre_diff_iq_dma_control'][
                        'traffic_pre_diff_iq_enable']),
            self.traffic_pre_diff_iq_registers[
                'traffic_pre_diff_iq_dma_status'][
                    'traffic_pre_diff_iq_overflow'].eq(
                self.traffic_pre_diff_iq_packer.overflow),
            self.traffic_pre_diff_iq_registers[
                'traffic_pre_diff_iq_dma_status']['last_buffer'].eq(
                self.traffic_pre_diff_iq_dma.last_buffer),
            self.traffic_pre_diff_iq_registers[
                'traffic_pre_diff_iq_next_address']['next_address'].eq(
                self.traffic_pre_diff_iq_dma.axi.awaddr),
        ]

        # ── Phase 10.7: wideband spectrometer ─────────────────────────
        # Direct instantiation of the Maia SDR `Spectrometer`
        # sub-module. Tap is pre-DDC off `rxiq_cdc` at AD9361 sample
        # rate (preset-dependent). The spectrometer's window + FFT +
        # integrator produce one 4096-bin spectrum per completed
        # integration to `wideband_spec_dma` at 0x2100_0000.
        #
        # `rxiq_cdc.re_out / im_out` are 12-bit unsigned wrappers of
        # 12-bit signed two's complement; reinterpret as signed.
        m.submodules.common_edge_2x = common_edge_2x = ClkNxCommonEdge(
            'sync', 'clk2x', 2)
        m.submodules.wideband_spec = self.wideband_spec
        m.submodules.wideband_spec_registers = self.wideband_spec_registers
        m.submodules.wideband_spec_registers_cdc = (
            wideband_spec_registers_cdc) = RegisterCDC(
                's_axi_lite', 'sync', self.wideband_spec_registers.aw)

        spec_ctrl = self.wideband_spec_registers['spec_control']
        spec_stat = self.wideband_spec_registers['spec_status']
        m.d.comb += [
            self.wideband_spec.re_in.eq(rxiq_cdc.re_out.as_signed()),
            self.wideband_spec.im_in.eq(rxiq_cdc.im_out.as_signed()),
            # Gate the strobe on `spec_enable` so the spectrometer can
            # be idled without tearing down the whole P25 chain.
            self.wideband_spec.strobe_in.eq(
                rxiq_cdc.strobe_out & spec_ctrl['spec_enable']),
            self.wideband_spec.common_edge_2x.eq(
                common_edge_2x.common_edge),
            self.wideband_spec.common_edge_3x.eq(
                common_edge_3x.common_edge),
            self.wideband_spec.number_integrations.eq(
                spec_ctrl['spec_num_integrations']),
            self.wideband_spec.peak_detect.eq(
                spec_ctrl['spec_peak_detect']),
            self.wideband_spec.abort.eq(spec_ctrl['spec_abort']),
            spec_stat['spec_last_buffer'].eq(
                self.wideband_spec.last_buffer),
            self.wideband_spec_registers[
                'spec_next_address']['next_address'].eq(
                self.wideband_spec.dma.axi.awaddr),
        ]

        # ── Register crossbar ─────────────────────────────────────────
        # Address map (word-addressed via AXI4-Lite, 8-bit address):
        # Bank field is bits [6:3] of the word address (4 bits = 16
        # banks max). See doc/P25_ADDRESS_MAP.md for the canonical table.
        #
        # Phase 10.8 layout:
        #   word 0x00-0x07: control          (bank 0)
        #   word 0x08-0x0F: SDR/DDC          (bank 1)
        #   word 0x18-0x1F: traffic          (bank 3)
        #   word 0x20-0x27: IQ DMA           (bank 4)
        #   word 0x28-0x2F: LSM              (bank 5)
        #   word 0x30-0x37: traffic_lsm      (bank 6)
        #   word 0x38-0x3F: traffic_iq       (bank 7)
        #   word 0x60-0x67: spectrometer     (bank 12)
        #   word 0x68-0x6F: pre_diff_iq      (bank 13)
        #   word 0x70-0x77: traffic_pre_diff_iq (bank 14)
        # Banks 2, 8-11 and 15 are intentionally vacant.
        address = Signal(self.axi4_awidth, reset_less=True)
        wdata = Signal(32, reset_less=True)
        addr_bank = self.axi4lite.address[3:7]  # bits [6:3]
        control_regs_select = (addr_bank == 0b0000)
        sdr_regs_select = (addr_bank == 0b0001)
        traffic_regs_select = (addr_bank == 0b0011)
        iq_regs_select = (addr_bank == 0b0100)
        lsm_regs_select = (addr_bank == 0b0101)
        traffic_lsm_regs_select = (addr_bank == 0b0110)
        traffic_iq_regs_select = (addr_bank == 0b0111)
        spec_regs_select = (addr_bank == 0b1100)
        pre_diff_iq_regs_select = (addr_bank == 0b1101)
        traffic_pre_diff_iq_regs_select = (addr_bank == 0b1110)

        m.d.s_axi_lite += [
            self.axi4lite.rdata.eq(self.control_registers.rdata
                                   | sdr_registers_cdc.i_rdata
                                   | traffic_registers_cdc.i_rdata
                                   | iq_registers_cdc.i_rdata
                                   | lsm_registers_cdc.i_rdata
                                   | traffic_lsm_registers_cdc.i_rdata
                                   | traffic_iq_registers_cdc.i_rdata
                                   | pre_diff_iq_registers_cdc.i_rdata
                                   | traffic_pre_diff_iq_registers_cdc.i_rdata
                                   | wideband_spec_registers_cdc.i_rdata),
            self.axi4lite.rdone.eq(self.control_registers.rdone
                                   | sdr_registers_cdc.i_rdone
                                   | traffic_registers_cdc.i_rdone
                                   | iq_registers_cdc.i_rdone
                                   | lsm_registers_cdc.i_rdone
                                   | traffic_lsm_registers_cdc.i_rdone
                                   | traffic_iq_registers_cdc.i_rdone
                                   | pre_diff_iq_registers_cdc.i_rdone
                                   | traffic_pre_diff_iq_registers_cdc.i_rdone
                                   | wideband_spec_registers_cdc.i_rdone),
            self.axi4lite.wdone.eq(self.control_registers.wdone
                                   | sdr_registers_cdc.i_wdone
                                   | traffic_registers_cdc.i_wdone
                                   | iq_registers_cdc.i_wdone
                                   | lsm_registers_cdc.i_wdone
                                   | traffic_lsm_registers_cdc.i_wdone
                                   | traffic_iq_registers_cdc.i_wdone
                                   | pre_diff_iq_registers_cdc.i_wdone
                                   | traffic_pre_diff_iq_registers_cdc.i_wdone
                                   | wideband_spec_registers_cdc.i_wdone),
            self.control_registers.ren.eq(
                self.axi4lite.ren & control_regs_select),
            self.control_registers.wstrobe.eq(
                Mux(control_regs_select, self.axi4lite.wstrobe, 0)),
            sdr_registers_cdc.i_ren.eq(
                self.axi4lite.ren & sdr_regs_select),
            sdr_registers_cdc.i_wstrobe.eq(
                Mux(sdr_regs_select, self.axi4lite.wstrobe, 0)),
            traffic_registers_cdc.i_ren.eq(
                self.axi4lite.ren & traffic_regs_select),
            traffic_registers_cdc.i_wstrobe.eq(
                Mux(traffic_regs_select, self.axi4lite.wstrobe, 0)),
            iq_registers_cdc.i_ren.eq(
                self.axi4lite.ren & iq_regs_select),
            iq_registers_cdc.i_wstrobe.eq(
                Mux(iq_regs_select, self.axi4lite.wstrobe, 0)),
            lsm_registers_cdc.i_ren.eq(
                self.axi4lite.ren & lsm_regs_select),
            lsm_registers_cdc.i_wstrobe.eq(
                Mux(lsm_regs_select, self.axi4lite.wstrobe, 0)),
            traffic_lsm_registers_cdc.i_ren.eq(
                self.axi4lite.ren & traffic_lsm_regs_select),
            traffic_lsm_registers_cdc.i_wstrobe.eq(
                Mux(traffic_lsm_regs_select, self.axi4lite.wstrobe, 0)),
            traffic_iq_registers_cdc.i_ren.eq(
                self.axi4lite.ren & traffic_iq_regs_select),
            traffic_iq_registers_cdc.i_wstrobe.eq(
                Mux(traffic_iq_regs_select, self.axi4lite.wstrobe, 0)),
            wideband_spec_registers_cdc.i_ren.eq(
                self.axi4lite.ren & spec_regs_select),
            wideband_spec_registers_cdc.i_wstrobe.eq(
                Mux(spec_regs_select, self.axi4lite.wstrobe, 0)),
            # Phase 10.8 bank selects
            pre_diff_iq_registers_cdc.i_ren.eq(
                self.axi4lite.ren & pre_diff_iq_regs_select),
            pre_diff_iq_registers_cdc.i_wstrobe.eq(
                Mux(pre_diff_iq_regs_select, self.axi4lite.wstrobe, 0)),
            traffic_pre_diff_iq_registers_cdc.i_ren.eq(
                self.axi4lite.ren & traffic_pre_diff_iq_regs_select),
            traffic_pre_diff_iq_registers_cdc.i_wstrobe.eq(
                Mux(traffic_pre_diff_iq_regs_select,
                    self.axi4lite.wstrobe, 0)),
            address.eq(self.axi4lite.address),
            wdata.eq(self.axi4lite.wdata),
        ]
        m.d.comb += [
            self.control_registers.address.eq(address),
            self.control_registers.wdata.eq(wdata),
            sdr_registers_cdc.i_address.eq(address),
            sdr_registers_cdc.i_wdata.eq(wdata),
            traffic_registers_cdc.i_address.eq(address),
            traffic_registers_cdc.i_wdata.eq(wdata),
            iq_registers_cdc.i_address.eq(address),
            iq_registers_cdc.i_wdata.eq(wdata),
            lsm_registers_cdc.i_address.eq(address),
            lsm_registers_cdc.i_wdata.eq(wdata),
            traffic_lsm_registers_cdc.i_address.eq(address),
            traffic_lsm_registers_cdc.i_wdata.eq(wdata),
            traffic_iq_registers_cdc.i_address.eq(address),
            traffic_iq_registers_cdc.i_wdata.eq(wdata),
            pre_diff_iq_registers_cdc.i_address.eq(address),
            pre_diff_iq_registers_cdc.i_wdata.eq(wdata),
            traffic_pre_diff_iq_registers_cdc.i_address.eq(address),
            traffic_pre_diff_iq_registers_cdc.i_wdata.eq(wdata),
            wideband_spec_registers_cdc.i_address.eq(address),
            wideband_spec_registers_cdc.i_wdata.eq(wdata),
        ]

        # ── Registers sync domain ────────────────────────────────────
        # sdr_registers CDC
        m.d.comb += [
            self.sdr_registers.ren.eq(sdr_registers_cdc.o_ren),
            self.sdr_registers.wstrobe.eq(sdr_registers_cdc.o_wstrobe),
            self.sdr_registers.address.eq(sdr_registers_cdc.o_address),
            self.sdr_registers.wdata.eq(sdr_registers_cdc.o_wdata),
            sdr_registers_cdc.o_rdone.eq(self.sdr_registers.rdone),
            sdr_registers_cdc.o_wdone.eq(self.sdr_registers.wdone),
            sdr_registers_cdc.o_rdata.eq(self.sdr_registers.rdata),
        ]
        # traffic_registers CDC
        m.d.comb += [
            self.traffic_registers.ren.eq(traffic_registers_cdc.o_ren),
            self.traffic_registers.wstrobe.eq(traffic_registers_cdc.o_wstrobe),
            self.traffic_registers.address.eq(traffic_registers_cdc.o_address),
            self.traffic_registers.wdata.eq(traffic_registers_cdc.o_wdata),
            traffic_registers_cdc.o_rdone.eq(self.traffic_registers.rdone),
            traffic_registers_cdc.o_wdone.eq(self.traffic_registers.wdone),
            traffic_registers_cdc.o_rdata.eq(self.traffic_registers.rdata),
        ]
        # iq_registers CDC (Phase 6C)
        m.d.comb += [
            self.iq_registers.ren.eq(iq_registers_cdc.o_ren),
            self.iq_registers.wstrobe.eq(iq_registers_cdc.o_wstrobe),
            self.iq_registers.address.eq(iq_registers_cdc.o_address),
            self.iq_registers.wdata.eq(iq_registers_cdc.o_wdata),
            iq_registers_cdc.o_rdone.eq(self.iq_registers.rdone),
            iq_registers_cdc.o_wdone.eq(self.iq_registers.wdone),
            iq_registers_cdc.o_rdata.eq(self.iq_registers.rdata),
        ]
        # lsm_registers CDC (Phase 6E.9)
        m.d.comb += [
            self.lsm_registers.ren.eq(lsm_registers_cdc.o_ren),
            self.lsm_registers.wstrobe.eq(lsm_registers_cdc.o_wstrobe),
            self.lsm_registers.address.eq(lsm_registers_cdc.o_address),
            self.lsm_registers.wdata.eq(lsm_registers_cdc.o_wdata),
            lsm_registers_cdc.o_rdone.eq(self.lsm_registers.rdone),
            lsm_registers_cdc.o_wdone.eq(self.lsm_registers.wdone),
            lsm_registers_cdc.o_rdata.eq(self.lsm_registers.rdata),
        ]
        # traffic_lsm_registers CDC (Phase 7A.2)
        m.d.comb += [
            self.traffic_lsm_registers.ren.eq(
                traffic_lsm_registers_cdc.o_ren),
            self.traffic_lsm_registers.wstrobe.eq(
                traffic_lsm_registers_cdc.o_wstrobe),
            self.traffic_lsm_registers.address.eq(
                traffic_lsm_registers_cdc.o_address),
            self.traffic_lsm_registers.wdata.eq(
                traffic_lsm_registers_cdc.o_wdata),
            traffic_lsm_registers_cdc.o_rdone.eq(
                self.traffic_lsm_registers.rdone),
            traffic_lsm_registers_cdc.o_wdone.eq(
                self.traffic_lsm_registers.wdone),
            traffic_lsm_registers_cdc.o_rdata.eq(
                self.traffic_lsm_registers.rdata),
        ]
        # traffic_iq_registers CDC (2026-04-16 chain-symmetry fix)
        m.d.comb += [
            self.traffic_iq_registers.ren.eq(
                traffic_iq_registers_cdc.o_ren),
            self.traffic_iq_registers.wstrobe.eq(
                traffic_iq_registers_cdc.o_wstrobe),
            self.traffic_iq_registers.address.eq(
                traffic_iq_registers_cdc.o_address),
            self.traffic_iq_registers.wdata.eq(
                traffic_iq_registers_cdc.o_wdata),
            traffic_iq_registers_cdc.o_rdone.eq(
                self.traffic_iq_registers.rdone),
            traffic_iq_registers_cdc.o_wdone.eq(
                self.traffic_iq_registers.wdone),
            traffic_iq_registers_cdc.o_rdata.eq(
                self.traffic_iq_registers.rdata),
        ]
        # Phase 10.8: pre_diff_iq + traffic_pre_diff_iq register CDCs.
        m.d.comb += [
            self.pre_diff_iq_registers.ren.eq(
                pre_diff_iq_registers_cdc.o_ren),
            self.pre_diff_iq_registers.wstrobe.eq(
                pre_diff_iq_registers_cdc.o_wstrobe),
            self.pre_diff_iq_registers.address.eq(
                pre_diff_iq_registers_cdc.o_address),
            self.pre_diff_iq_registers.wdata.eq(
                pre_diff_iq_registers_cdc.o_wdata),
            pre_diff_iq_registers_cdc.o_rdone.eq(
                self.pre_diff_iq_registers.rdone),
            pre_diff_iq_registers_cdc.o_wdone.eq(
                self.pre_diff_iq_registers.wdone),
            pre_diff_iq_registers_cdc.o_rdata.eq(
                self.pre_diff_iq_registers.rdata),
        ]
        m.d.comb += [
            self.traffic_pre_diff_iq_registers.ren.eq(
                traffic_pre_diff_iq_registers_cdc.o_ren),
            self.traffic_pre_diff_iq_registers.wstrobe.eq(
                traffic_pre_diff_iq_registers_cdc.o_wstrobe),
            self.traffic_pre_diff_iq_registers.address.eq(
                traffic_pre_diff_iq_registers_cdc.o_address),
            self.traffic_pre_diff_iq_registers.wdata.eq(
                traffic_pre_diff_iq_registers_cdc.o_wdata),
            traffic_pre_diff_iq_registers_cdc.o_rdone.eq(
                self.traffic_pre_diff_iq_registers.rdone),
            traffic_pre_diff_iq_registers_cdc.o_wdone.eq(
                self.traffic_pre_diff_iq_registers.wdone),
            traffic_pre_diff_iq_registers_cdc.o_rdata.eq(
                self.traffic_pre_diff_iq_registers.rdata),
        ]
        m.d.comb += [
            self.wideband_spec_registers.ren.eq(
                wideband_spec_registers_cdc.o_ren),
            self.wideband_spec_registers.wstrobe.eq(
                wideband_spec_registers_cdc.o_wstrobe),
            self.wideband_spec_registers.address.eq(
                wideband_spec_registers_cdc.o_address),
            self.wideband_spec_registers.wdata.eq(
                wideband_spec_registers_cdc.o_wdata),
            wideband_spec_registers_cdc.o_rdone.eq(
                self.wideband_spec_registers.rdone),
            wideband_spec_registers_cdc.o_wdone.eq(
                self.wideband_spec_registers.wdone),
            wideband_spec_registers_cdc.o_rdata.eq(
                self.wideband_spec_registers.rdata),
        ]

        # ── Internal resets ───────────────────────────────────────────
        # Phase 10.7: include clk2x (driven by the new PS7 FCLK for
        # the wideband spectrometer's Blackman-Harris window + FFT
        # twiddle path).
        for internal in ['sync', 'clk3x', 'clk2x', 'sampling']:
            setattr(m.submodules, f'{internal}_rst', FFSynchronizer(
                self.control_registers['control']['sdr_reset'],
                ResetSignal(internal), o_domain=internal,
                init=1))
        m.d.comb += rxiq_cdc.reset.eq(
            self.control_registers['control']['sdr_reset'])

        # ── Interrupts ────────────────────────────────────────────────
        m.d.comb += [
            self.interrupt_out.eq(interrupts_reg.interrupt),
        ]

        return m


def write_svd(path):
    top = P25Core()
    with open(path, 'wb') as f:
        f.write(top.svd())


def parse_args():
    parser = argparse.ArgumentParser(
        description='Generate Fishball P25 IP core Verilog')
    parser.add_argument(
        '--config', default='default',
        help='P25 configuration name [default=%(default)r]')
    parser.add_argument(
        'output_file', help='Output Verilog file')
    return parser.parse_args()


def main():
    args = parse_args()
    config = getattr(configs, args.config)()
    top = P25Core(config)
    platform = PlutoPlatform()
    with open(args.output_file, 'w') as f:
        f.write(amaranth.back.verilog.convert(
            top, platform=platform, ports=top.ports()))


if __name__ == '__main__':
    main()
