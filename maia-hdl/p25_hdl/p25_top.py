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
#   Traffic side, chain 2 (core 0.3.0, doc/changes/064; no diagnostic taps):
#     traffic2_ddc -> traffic2_lsm chain -> traffic2_lsm_dibit_packer -> traffic2_lsm_dibit_dma (0x1D00_0000)
#
# Recovered NIDs from each LSM chain are surfaced via separate AXI
# register banks: `lsm` at 0xA0 (control side, bank 5),
# `traffic_lsm` at 0xC0 (traffic side, bank 6) and `traffic2_lsm` at
# 0x140 (traffic chain 2, banks 10-11, which also hold its seeds).
#
# LSM chain pipeline (control vs. traffic — different rates 2026-05-03):
#
#   Control chain (31.25 kSPS, unchanged):
#     P25DDC out (62.5 kSPS)
#       -> LsmDecimator2  (/2)             -> 31.25 kSPS
#       -> LsmFir(LPF_TAPS_31250)          -- 83-tap baseband LPF
#       -> LsmFir(RRC_TAPS_31250)          -- 105-tap matched filter (alpha=0.2)
#       -> LsmDemod(sample_rate_hz=31_250) -- timing recovery + Costas PLL +
#                                             pre-diff rotate + interleave +
#                                             diff slicer + sync detect + BCH
#
#   Traffic chain (25 kSPS, post-2026-05-03; matches SDRTrunk-bit-exact PS):
#     polyphase_channelizer + per_target_ddc + mux  (8 MSPS / 64 / 5 = 25 kSPS)
#       -> LsmFir(LPF_TAPS_25K)            -- 121-tap baseband LPF
#       -> LsmFir(RRC_TAPS_25K)            -- 42-tap RRC (SDRTrunk-exact)
#       -> LsmDemod(sample_rate_hz=25_000) -- same demod pipeline; the
#                                             timing-recovery + Gardner
#                                             constants track sps via the
#                                             sample_rate_hz parameter.
#
#   Both chains drive the same downstream pattern:
#       +--> dibit_out / symbol_strobe   -> {lsm,traffic_lsm}_dibit_packer
#                                          -> {lsm,traffic_lsm}_dibit_dma
#       +--> i_pre_diff_out / q_pre_diff_out / pre_diff_strobe_out (control only)
#                                          -> pre_diff_iq_packer -> pre_diff_iq_dma
#       +--> nid_event_strobe + (NAC, DUID, n_errors, ...)
#                                          -> `lsm` / `traffic_lsm` register bank
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
from .lsm_fir import (
    LsmFir,
    LPF_TAPS_31250, RRC_TAPS_31250,
    LPF_TAPS_25K, RRC_TAPS_25K,
)
from .lsm_demod import LsmDemod
# 2026-05-03 dual-DDC pivot: polyphase channelizer / per_target_ddc /
# traffic_pipeline / signal_energy modules retired from this top, kept
# in tree for a future scanner mode. Re-import here when re-enabling.
# from .polyphase_channelizer import PolyphaseChannelizer
# from .signal_energy import SignalEnergy
# from .polyphase_proto_coeffs import PROTO_COEFFS, M as PROTO_M, K as PROTO_K
# from .per_target_ddc import design_decim_fir
# from .traffic_pipeline import TrafficPipeline
from .config import P25Config
from . import configs

# IP core version
# 0.2.0: LSM PLL/timing no-signal hold (doc/changes/059).
# 0.3.0: second traffic decode chain `traffic2_*` (doc/changes/064).
#        Register map is a strict superset of 0.2.0.
_version = '0.3.0'


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
                    # Phase 10.8 2026-04-22: control-side pre-diff IQ
                    # ring DMA. Tapped inside LsmDemodLoop after PLL
                    # rotate + mid/sym interleave, before the
                    # differential slicer. Feeds the Plots tab.
                    Field('pre_diff_iq_dma', Access.Rsticky, 1, 0),
                    # Phase 10.7: wideband spectrometer.
                    Field('wideband_spec_dma', Access.Rsticky, 1, 0),
                    # 2026-05-03 dual-DDC: traffic-side LSM dibit ring
                    # DMA (fed by traffic_ddc -> LsmDecimator2 -> LPF
                    # -> RRC -> LsmDemod, mirroring the control chain).
                    Field('traffic_lsm_dibit_dma',
                          Access.Rsticky, 1, 0),
                    # 2026-05-03 dual-DDC parity: traffic-side post-DDC
                    # IQ ring DMA. Mirror of `iq_dma` on the traffic
                    # chain; feeds /api/spectrum?chain=traffic.
                    Field('traffic_iq_dma',
                          Access.Rsticky, 1, 0),
                    # 2026-05-03 dual-DDC parity: traffic-side pre-diff
                    # post-PLL IQ ring DMA. Mirror of `pre_diff_iq_dma`
                    # on the traffic chain; feeds Plots tab traffic-
                    # side eye / constellation / deviation views.
                    Field('traffic_pre_diff_iq_dma',
                          Access.Rsticky, 1, 0),
                    # 2026-05-03: pre-DDC raw 8 MSPS / 8 MHz BW IQ
                    # tap, fed by rxiq_cdc directly. Diagnostic /
                    # offline-capture tap (e.g. .cs16 dump for
                    # SDRTrunk reference comparison).
                    Field('wideband_iq_dma',
                          Access.Rsticky, 1, 0),
                    # Core 0.3.0 (doc/changes/064): traffic chain 2
                    # LSM dibit ring DMA. Bit 8, appended so bits 0-7
                    # keep their 0.2.0 positions.
                    Field('traffic2_lsm_dibit_dma',
                          Access.Rsticky, 1, 0),
                ], interrupt=True),
            },
            2)

        # ── Control DDC registers (0x08) ──────────────────────────────
        # P25DDC: SDRTrunk-faithful v2 fork of maia_hdl.ddc.DDC with
        # unit-DC-gain coefficient convention and tightened stage 3
        # filter to fix the LsmDecimator2 fold-back bug. See
        # p25_hdl/p25ddc.py and doc/changes/041_p25ddc_fork.md.
        # 2026-05-03: dual-DDC pivot — control + traffic each get
        # their own independent P25DDC instance (NCO + per-stage FIR
        # coeffs are programmed independently from PS), restoring
        # the pre-channelizer architecture. Polyphase / per_target_ddc
        # / traffic_pipeline modules remain in tree (see
        # `polyphase_channelizer.py` etc.) for future scanner mode but
        # are not instantiated here.
        self.ddc = P25DDC('clk3x')
        self.traffic_ddc = P25DDC('clk3x')
        # Core 0.3.0: second traffic chain's DDC (doc/changes/064).
        self.traffic2_ddc = P25DDC('clk3x')

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

        # ── Traffic DDC registers (bank 2 @ 0x40) ────────────────────
        # Mirror of `sdr_registers` (control DDC) so the PS can
        # program the traffic DDC's NCO + per-stage FIR coefficients
        # independently. Field names get a `traffic_` prefix so the
        # SVD-generated PAC reads naturally.
        self.traffic_sdr_registers = Registers(
            'traffic_sdr', {
                0b000: Register(
                    'traffic_ddc_coeff_addr', [
                        Field('traffic_coeff_waddr', Access.RW, 10, 0),
                    ]),
                0b010: Register(
                    'traffic_ddc_coeff', [
                        Field('traffic_coeff_wren', Access.Wpulse, 1, 0),
                        Field('traffic_coeff_wdata', Access.RW, 18, 0),
                    ]),
                0b011: Register(
                    'traffic_ddc_decimation', [
                        Field('traffic_decimation1', Access.RW, 7, 0),
                        Field('traffic_decimation2', Access.RW, 6, 0),
                        Field('traffic_decimation3', Access.RW, 7, 0),
                    ]),
                0b100: Register(
                    'traffic_ddc_frequency', [
                        Field('traffic_frequency', Access.RW, 28, 0),
                    ]),
                0b101: Register(
                    'traffic_ddc_control', [
                        Field('traffic_operations_minus_one1', Access.RW, 7, 0),
                        Field('traffic_operations_minus_one2', Access.RW, 6, 0),
                        Field('traffic_operations_minus_one3', Access.RW, 7, 0),
                        Field('traffic_odd_operations1', Access.RW, 1, 0),
                        Field('traffic_odd_operations3', Access.RW, 1, 0),
                        Field('traffic_bypass2', Access.RW, 1, 0),
                        Field('traffic_bypass3', Access.RW, 1, 0),
                        Field('traffic_enable_input', Access.RW, 1, 0),
                    ]),
            }, 3)

        # ── Traffic chain 2 DDC registers (bank 9 @ 0x120, core 0.3.0) ─
        # Mirror of `traffic_sdr_registers` for `traffic2_ddc`, same
        # word offsets inside the bank, `traffic2_` prefix. The PS
        # programs chain 2's NCO + per-stage FIR coefficients
        # independently of chain 1 (doc/changes/064).
        self.traffic2_sdr_registers = Registers(
            'traffic2_sdr', {
                0b000: Register(
                    'traffic2_ddc_coeff_addr', [
                        Field('traffic2_coeff_waddr', Access.RW, 10, 0),
                    ]),
                0b010: Register(
                    'traffic2_ddc_coeff', [
                        Field('traffic2_coeff_wren', Access.Wpulse, 1, 0),
                        Field('traffic2_coeff_wdata', Access.RW, 18, 0),
                    ]),
                0b011: Register(
                    'traffic2_ddc_decimation', [
                        Field('traffic2_decimation1', Access.RW, 7, 0),
                        Field('traffic2_decimation2', Access.RW, 6, 0),
                        Field('traffic2_decimation3', Access.RW, 7, 0),
                    ]),
                0b100: Register(
                    'traffic2_ddc_frequency', [
                        Field('traffic2_frequency', Access.RW, 28, 0),
                    ]),
                0b101: Register(
                    'traffic2_ddc_control', [
                        Field('traffic2_operations_minus_one1',
                              Access.RW, 7, 0),
                        Field('traffic2_operations_minus_one2',
                              Access.RW, 6, 0),
                        Field('traffic2_operations_minus_one3',
                              Access.RW, 7, 0),
                        Field('traffic2_odd_operations1', Access.RW, 1, 0),
                        Field('traffic2_odd_operations3', Access.RW, 1, 0),
                        Field('traffic2_bypass2', Access.RW, 1, 0),
                        Field('traffic2_bypass3', Access.RW, 1, 0),
                        Field('traffic2_enable_input', Access.RW, 1, 0),
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
        # C4FM path). Consumes the control DDC output (50 kSPS post
        # 2026-05-03 retune, 16-bit signed I+Q), decimates by 2 down
        # to 25 kSPS, filters with the 121-tap baseband LPF and the
        # 42-tap SDRTrunk-exact RRC matched filter, then runs LsmDemod
        # (timing recovery + Costas PLL rotate + mid/sym interleave +
        # diff demod + slicer + sync detect + BCH FEC). The recovered
        # dibits exit via lsm_dibit_dma; the recovered NIDs are
        # surfaced via the `lsm` register bank.
        #
        # 2026-05-03: rate dropped 31.25 → 25 kSPS to match SDRTrunk's
        # native LSM front-end rate. DDC presets regenerated to land
        # at 50 kSPS DDC output (was 62.5); LsmDecimator2 /2 unchanged;
        # LSM front-end filters swapped to the 25K variants;
        # LsmDemod parameterised by sample_rate_hz.
        self.lsm_decimator = LsmDecimator2(width=16)
        self.lsm_lpf = LsmFir(LPF_TAPS_25K)
        self.lsm_rrc = LsmFir(RRC_TAPS_25K)
        self.lsm_demod = LsmDemod(sample_rate_hz=25_000)
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

        # ── Wideband raw IQ DMA (2026-05-03) ──────────────────────────
        # Pre-DDC tap of `rxiq_cdc.{re_out,im_out,strobe_out}` at 8 MSPS
        # (12-bit signed I/Q sign-extended to 16-bit). Same packer +
        # ring DMA pattern as `iq_dma`, just a much larger ring (16 MB
        # = 0.5 s) because the byte rate is 128x higher (32 MB/s vs
        # 250 KB/s).
        self.wideband_iq_packer = IQPacker()
        self.wideband_iq_dma = DmaStreamRingWrite(
            config.wideband_iq_dma_address,
            config.wideband_iq_dma_num_buffers_log2,
            config.wideband_iq_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_wideband_iq')
        self.wideband_iq_registers = Registers(
            'wideband_iq', {
                0b00: Register('wideband_iq_dma_status', [
                    Field('wideband_iq_overflow', Access.Rsticky, 1, 0),
                    Field('last_buffer', Access.R,
                          config.wideband_iq_dma_num_buffers_log2, -1),
                ]),
                0b01: Register('wideband_iq_dma_control', [
                    Field('wideband_iq_enable', Access.RW, 1, 0),
                ]),
                0b10: Register('wideband_iq_next_address', [
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

        # ── Traffic LSM demod chain (dual-DDC pivot, 2026-05-03) ─────
        # Pipeline (mirrors the control chain block-for-block):
        #   traffic_ddc out (50 kSPS post-2026-05-03 retune)
        #     -> LsmDecimator2 /2     (-> 25 kSPS)
        #     -> LsmFir(LPF_TAPS_25K) (121-tap baseband LPF)
        #     -> LsmFir(RRC_TAPS_25K) (42-tap SDRTrunk-exact RRC)
        #     -> LsmDemod(sample_rate_hz=25_000)
        #         -> { dibit_packer -> traffic_lsm_dibit_dma,
        #              NID event registers }
        #
        # Why dual-DDC instead of polyphase channelizer + per-target
        # DDC: testing 2026-05-02/03 showed the polyphase path didn't
        # land any of the audio-quality wins it promised, while the
        # extra runtime gating (per-target enable / NCO programming)
        # destabilised live decode. Dual-DDC matches SDRTrunk's
        # baseline architecture (one heterodyne + decimate per
        # channel) and is what the operator's PS-side stack already
        # assumed. Polyphase + per_target_ddc + traffic_pipeline
        # modules remain in tree for a future scanner mode (multi-
        # traffic-channel monitoring) but are not instantiated here.
        self.traffic_lsm_decimator = LsmDecimator2(width=16)
        self.traffic_lsm_lpf = LsmFir(LPF_TAPS_25K)
        self.traffic_lsm_rrc = LsmFir(RRC_TAPS_25K)
        self.traffic_lsm_demod = LsmDemod(sample_rate_hz=25_000)
        self.traffic_lsm_dibit_packer = DibitPacker()
        self.traffic_lsm_dibit_dma = DmaStreamRingWrite(
            config.traffic_lsm_dibit_dma_address,
            config.traffic_lsm_dibit_dma_num_buffers_log2,
            config.traffic_lsm_dibit_dma_buffer_size,
            width=64, axi_awidth=32,
            name='m_axi_traffic_lsm_dibit')

        # ── Traffic LSM register bank (0xC0, bank 6) ─────────────────
        # Mirror of `lsm_registers` (bank 5). Field names use the
        # `traffic_lsm_*` prefix so the SVD-generated PAC reads
        # naturally on the PS side.
        self.traffic_lsm_registers = Registers(
            'traffic_lsm', {
                0b000: Register('traffic_lsm_control', [
                    Field('traffic_lsm_enable', Access.RW, 1, 0),
                    Field('traffic_lsm_dibit_dma_enable',
                          Access.RW, 1, 0),
                    Field('traffic_lsm_reset', Access.Wpulse, 1, 0),
                    Field('traffic_lsm_dc_block_enable',
                          Access.RW, 1, 0),
                    Field('traffic_lsm_agc_enable', Access.RW, 1, 0),
                ]),
                0b001: Register('traffic_lsm_status', [
                    Field('bch_busy', Access.R, 1, 0),
                    Field('in_nid_window', Access.R, 1, 0),
                    Field('nid_event', Access.Rsticky, 1, 0),
                    Field('nid_valid', Access.R, 1, 0),
                    Field('n_errors', Access.R, 7, 0),
                    Field('sync_distance', Access.R, 7, 0),
                    Field('traffic_lsm_dibit_overflow',
                          Access.Rsticky, 1, 0),
                ]),
                0b010: Register('traffic_lsm_nid', [
                    Field('nac', Access.R, 12, 0),
                    Field('duid', Access.R, 4, 0),
                ]),
                0b011: Register('traffic_lsm_drop_count', [
                    Field('drop_count', Access.R, 16, 0),
                    Field('traffic_lsm_dibit_last_buffer', Access.R,
                          config.traffic_lsm_dibit_dma_num_buffers_log2,
                          -1),
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
                0b111: Register('traffic_lsm_agc_config', [
                    Field('mag_update_threshold', Access.RW, 16, 256),
                ]),
            },
            3)

        # ── Traffic chain 2 LSM demod chain (core 0.3.0) ──────────────
        # Second traffic decode chain so the PS can follow two voice
        # calls at once (doc/changes/064). Block-for-block copy of the
        # traffic chain, including the 059 PLL/timing no-signal hold
        # (LsmDemod defaults):
        #   traffic2_ddc out (50 kSPS)
        #     -> LsmDecimator2 /2     (-> 25 kSPS)
        #     -> LsmFir(LPF_TAPS_25K) (121-tap baseband LPF)
        #     -> LsmFir(RRC_TAPS_25K) (42-tap SDRTrunk-exact RRC)
        #     -> LsmDemod(sample_rate_hz=25_000)
        #         -> { dibit_packer -> traffic2_lsm_dibit_dma,
        #              NID event registers }
        # No post-DDC IQ or pre-diff IQ diagnostic taps on this chain.
        self.traffic2_lsm_decimator = LsmDecimator2(width=16)
        self.traffic2_lsm_lpf = LsmFir(LPF_TAPS_25K)
        self.traffic2_lsm_rrc = LsmFir(RRC_TAPS_25K)
        self.traffic2_lsm_demod = LsmDemod(sample_rate_hz=25_000)
        self.traffic2_lsm_dibit_packer = DibitPacker()
        self.traffic2_lsm_dibit_dma = DmaStreamRingWrite(
            config.traffic2_lsm_dibit_dma_address,
            config.traffic2_lsm_dibit_dma_num_buffers_log2,
            config.traffic2_lsm_dibit_dma_buffer_size,
            width=64, axi_awidth=32,
            name='m_axi_traffic2_lsm_dibit')

        # ── Traffic chain 2 LSM register bank (banks 10-11 @ 0x140) ──
        # One 16-word bank (4-bit word address). Words 0-7 mirror
        # `traffic_lsm_registers` word for word (`traffic2_lsm_*`
        # names); words 8-10 hold chain 2's warm-start seeds, the
        # counterpart of bank 8's `traffic_lsm_*_seed` registers (same
        # widths / Q-formats, latched on `traffic2_lsm_reset`). Keeping
        # the seeds in the same bank as the reset pulse means seed
        # writes and the reset cross the same RegisterCDC in order, so
        # chain 2 needs no read-back fence between them.
        self.traffic2_lsm_registers = Registers(
            'traffic2_lsm', {
                0b0000: Register('traffic2_lsm_control', [
                    Field('traffic2_lsm_enable', Access.RW, 1, 0),
                    Field('traffic2_lsm_dibit_dma_enable',
                          Access.RW, 1, 0),
                    Field('traffic2_lsm_reset', Access.Wpulse, 1, 0),
                    Field('traffic2_lsm_dc_block_enable',
                          Access.RW, 1, 0),
                    Field('traffic2_lsm_agc_enable', Access.RW, 1, 0),
                ]),
                0b0001: Register('traffic2_lsm_status', [
                    Field('bch_busy', Access.R, 1, 0),
                    Field('in_nid_window', Access.R, 1, 0),
                    Field('nid_event', Access.Rsticky, 1, 0),
                    Field('nid_valid', Access.R, 1, 0),
                    Field('n_errors', Access.R, 7, 0),
                    Field('sync_distance', Access.R, 7, 0),
                    Field('traffic2_lsm_dibit_overflow',
                          Access.Rsticky, 1, 0),
                ]),
                0b0010: Register('traffic2_lsm_nid', [
                    Field('nac', Access.R, 12, 0),
                    Field('duid', Access.R, 4, 0),
                ]),
                0b0011: Register('traffic2_lsm_drop_count', [
                    Field('drop_count', Access.R, 16, 0),
                    Field('traffic2_lsm_dibit_last_buffer', Access.R,
                          config.traffic2_lsm_dibit_dma_num_buffers_log2,
                          -1),
                ]),
                0b0100: Register('traffic2_lsm_dibit_next', [
                    Field('next_address', Access.R, 32, 0),
                ]),
                0b0101: Register('traffic2_lsm_debug', [
                    Field('pll_dbg', Access.R, 16, 0),
                    Field('sample_point_dbg', Access.R, 16, 0),
                ]),
                0b0110: Register('traffic2_lsm_agc_debug', [
                    Field('agc_gain_dbg', Access.R, 16, 0),
                    Field('agc_mag_dbg', Access.R, 16, 0),
                ]),
                0b0111: Register('traffic2_lsm_agc_config', [
                    Field('mag_update_threshold', Access.RW, 16, 256),
                ]),
                0b1000: Register('traffic2_lsm_agc_seed', [
                    Field('agc_seed', Access.RW, 20, 0),
                ]),
                0b1001: Register('traffic2_lsm_pll_seed', [
                    Field('pll_seed', Access.RW, 16, 0),
                ]),
                0b1010: Register('traffic2_lsm_timing_seed', [
                    Field('timing_seed', Access.RW, 18, 0),
                ]),
            },
            4)

        # ── LSM seed register bank (bank 8 @ 0x100, 2026-05-03 bake) ─
        # Warm-start seeds for the AGC / PLL / Gardner timing loops on
        # both LSM chains. Latched into the loop accumulators on the
        # respective `lsm_reset` / `traffic_lsm_reset` pulse via the
        # Mux(seed != 0, seed, init) pattern inside lsm_agc.py,
        # lsm_pll_update.py, and lsm_timing_interp.py. Zero -> legacy
        # cold start.
        #
        # PS workflow (p25-httpd):
        #   1. During clean LDU flow on the control chain, the
        #      heartbeat snapshots (pll_dbg, sample_point_dbg,
        #      gain_dbg<<4) into a per-freq cache.
        #   2. On `retune_traffic_chain`, the cached seeds are written
        #      into this bank BEFORE pulsing `traffic_lsm_reset`.
        #   3. Seeds remain latched in the registers between resets,
        #      so a write-once / pulse-many flow is supported.
        #
        # Q-formats (raw register bits; sign interpretation happens in
        # the PAC / PS code):
        #   agc_seed     : 20-bit unsigned Q9.11 (matches LsmAgc.gain
        #                  internal accumulator; gain_dbg is the Q9.7
        #                  truncation, so PS shifts gain_dbg << 4 to
        #                  recover the Q9.11 representation).
        #   pll_seed     : 16-bit signed Q2.13 (matches pll_reg).
        #   timing_seed  : 18-bit signed Q5.12 (matches sample_point).
        self.lsm_seed_registers = Registers(
            'lsm_seed', {
                0b000: Register('lsm_agc_seed', [
                    Field('agc_seed', Access.RW, 20, 0),
                ]),
                0b001: Register('lsm_pll_seed', [
                    Field('pll_seed', Access.RW, 16, 0),
                ]),
                0b010: Register('lsm_timing_seed', [
                    Field('timing_seed', Access.RW, 18, 0),
                ]),
                0b011: Register('traffic_lsm_agc_seed', [
                    Field('agc_seed', Access.RW, 20, 0),
                ]),
                0b100: Register('traffic_lsm_pll_seed', [
                    Field('pll_seed', Access.RW, 16, 0),
                ]),
                0b101: Register('traffic_lsm_timing_seed', [
                    Field('timing_seed', Access.RW, 18, 0),
                ]),
            },
            3)

        # ── Traffic-channel post-DDC IQ ring DMA (2026-05-03) ─────────
        # Mirror of the control `iq_dma`. Tapped off `traffic_ddc.re_out`
        # / `im_out` at the DDC output rate (50 kSPS post-2026-05-03).
        # Feeds /api/spectrum?chain=traffic + the traffic-side narrow-
        # band waterfall.
        self.traffic_iq_packer = IQPacker()
        self.traffic_iq_dma = DmaStreamRingWrite(
            config.traffic_iq_dma_address,
            config.traffic_iq_dma_num_buffers_log2,
            config.traffic_iq_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_traffic_iq')

        # ── Traffic IQ DMA registers (bank 3 @ 0x60) ──────────────────
        self.traffic_iq_registers = Registers(
            'traffic_iq', {
                0b00: Register('traffic_iq_dma_status', [
                    Field('traffic_iq_overflow', Access.Rsticky, 1, 0),
                    Field('traffic_iq_last_buffer', Access.R,
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

        # ── Traffic-chain pre-diff post-PLL IQ ring DMA (2026-05-03) ──
        # Mirror of the control `pre_diff_iq_dma`. Tapped inside the
        # traffic-side LsmDemod after LsmPllRotate but BEFORE the
        # differential slicer (i.e. AGC-scaled + carrier-derotated;
        # mid+sym interleave). Feeds the dashboard Plots tab when the
        # chain selector is set to `traffic`.
        self.traffic_pre_diff_iq_packer = IQPacker()
        self.traffic_pre_diff_iq_dma = DmaStreamRingWrite(
            config.traffic_pre_diff_iq_dma_address,
            config.traffic_pre_diff_iq_dma_num_buffers_log2,
            config.traffic_pre_diff_iq_dma_buffer_size,
            width=64, axi_awidth=32,
            name='m_axi_traffic_pre_diff_iq')
        self.traffic_pre_diff_iq_registers = Registers(
            'traffic_pre_diff_iq', {
                0b00: Register('traffic_pre_diff_iq_dma_status', [
                    Field('traffic_pre_diff_iq_overflow',
                          Access.Rsticky, 1, 0),
                    Field('traffic_pre_diff_iq_last_buffer', Access.R,
                          config.traffic_pre_diff_iq_dma_num_buffers_log2,
                          -1),
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

        # 2026-05-03 dual-DDC pivot: the polyphase channelizer +
        # per_target_ddc pool + traffic_pipeline + traffic_pipe
        # registers used to live here, fed by `rxiq_cdc` pre-DDC, and
        # produced one selected channel into `traffic_lsm_*`. They
        # have been removed in favour of a dedicated `traffic_ddc =
        # P25DDC(...)` mirroring the control DDC (instantiated near
        # `self.ddc` above).  The polyphase / per_target_ddc /
        # traffic_pipeline source modules remain in tree (unused) for
        # a future scanner mode.

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
        # (0x20). 2026-05-03 dual-DDC layout (full traffic-side parity
        # with control side):
        #
        #   0x00  bank 0   control       (product_id / version / ctrl / interrupts)
        #   0x20  bank 1   sdr           (control DDC config)
        #   0x40  bank 2   traffic_sdr   (traffic DDC config)
        #   0x60  bank 3   traffic_iq    (traffic post-DDC IQ DMA)
        #   0x80  bank 4   iq            (control post-DDC IQ DMA)
        #   0xA0  bank 5   lsm           (control LSM: dibit DMA + NID events)
        #   0xC0  bank 6   traffic_lsm   (traffic LSM: dibit DMA + NID events)
        #   0xE0  bank 7   wideband_iq   (raw 8 MSPS IQ DMA)
        #   0x100 bank 8   lsm_seed      (AGC/PLL/timing seeds, both chains)
        #   0x120 bank 9   traffic2_sdr  (traffic chain 2 DDC config, 0.3.0)
        #   0x140 bank 10-11 traffic2_lsm (traffic chain 2 LSM: dibit DMA +
        #                  NID events + seeds, 16-word bank, 0.3.0)
        #   0x180 bank 12  spectrometer  (wideband FFT DMA)
        #   0x1A0 bank 13  pre_diff_iq   (control pre-diff IQ DMA)
        #   0x1C0 bank 14  traffic_pre_diff_iq (traffic pre-diff IQ DMA)
        #
        # Vacant: 15.
        # See doc/P25_ADDRESS_MAP.md for the canonical bank table.
        self.register_map = RegisterMap({
            0x00:  self.control_registers,
            0x20:  self.sdr_registers,
            0x40:  self.traffic_sdr_registers,
            0x60:  self.traffic_iq_registers,
            0x80:  self.iq_registers,
            0xA0:  self.lsm_registers,
            0xC0:  self.traffic_lsm_registers,
            0xE0:  self.wideband_iq_registers,
            0x100: self.lsm_seed_registers,
            0x120: self.traffic2_sdr_registers,
            0x140: self.traffic2_lsm_registers,
            0x180: self.wideband_spec_registers,
            0x1A0: self.pre_diff_iq_registers,
            0x1C0: self.traffic_pre_diff_iq_registers,
        }, metadata)

        # ── I/O signals ────────────────────────────────────────────────
        self.iq_in_width = 12
        self.re_in = Signal(self.iq_in_width)
        self.im_in = Signal(self.iq_in_width)
        self.interrupt_out = Signal()

    def ports(self):
        return (
            self.axi4lite.axi.ports()
            + self.iq_dma.axi.ports()       # Phase 6C
            + self.lsm_dibit_dma.axi.ports()  # Phase 6E.9
            + self.pre_diff_iq_dma.axi.ports()           # Phase 10.8
            + self.wideband_spec.dma.axi.ports()    # Phase 10.7
            + self.traffic_lsm_dibit_dma.axi.ports()     # M2B 2026-05-02
            + self.traffic_iq_dma.axi.ports()            # 2026-05-03
            + self.traffic_pre_diff_iq_dma.axi.ports()   # 2026-05-03
            + self.wideband_iq_dma.axi.ports()           # 2026-05-03
            + self.traffic2_lsm_dibit_dma.axi.ports()    # 0.3.0 (064)
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
        m.submodules.traffic_ddc = self.traffic_ddc
        m.submodules.sdr_registers = self.sdr_registers
        m.submodules.sdr_registers_cdc = sdr_registers_cdc = RegisterCDC(
            's_axi_lite', 'sync', self.sdr_registers.aw)
        m.submodules.traffic_sdr_registers = self.traffic_sdr_registers
        m.submodules.traffic_sdr_registers_cdc = (
            traffic_sdr_registers_cdc) = RegisterCDC(
                's_axi_lite', 'sync', self.traffic_sdr_registers.aw)
        # Core 0.3.0: traffic chain 2 DDC + its register bank.
        m.submodules.traffic2_ddc = self.traffic2_ddc
        m.submodules.traffic2_sdr_registers = self.traffic2_sdr_registers
        m.submodules.traffic2_sdr_registers_cdc = (
            traffic2_sdr_registers_cdc) = RegisterCDC(
                's_axi_lite', 'sync', self.traffic2_sdr_registers.aw)

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

        # ── Traffic DDC (mirror of control DDC, dual-DDC pivot 2026-05-03)
        m.d.comb += [
            self.traffic_ddc.common_edge.eq(common_edge_3x.common_edge),
            self.traffic_ddc.enable_input.eq(
                self.traffic_sdr_registers['traffic_ddc_control']['traffic_enable_input']),
            self.traffic_ddc.frequency.eq(
                self.traffic_sdr_registers['traffic_ddc_frequency']['traffic_frequency']),
            self.traffic_ddc.coeff_waddr.eq(
                self.traffic_sdr_registers['traffic_ddc_coeff_addr']['traffic_coeff_waddr']),
            self.traffic_ddc.coeff_wren.eq(
                self.traffic_sdr_registers['traffic_ddc_coeff']['traffic_coeff_wren']),
            self.traffic_ddc.coeff_wdata.eq(
                self.traffic_sdr_registers['traffic_ddc_coeff']['traffic_coeff_wdata']),
            self.traffic_ddc.decimation1.eq(
                self.traffic_sdr_registers['traffic_ddc_decimation']['traffic_decimation1']),
            self.traffic_ddc.decimation2.eq(
                self.traffic_sdr_registers['traffic_ddc_decimation']['traffic_decimation2']),
            self.traffic_ddc.decimation3.eq(
                self.traffic_sdr_registers['traffic_ddc_decimation']['traffic_decimation3']),
            self.traffic_ddc.bypass2.eq(
                self.traffic_sdr_registers['traffic_ddc_control']['traffic_bypass2']),
            self.traffic_ddc.bypass3.eq(
                self.traffic_sdr_registers['traffic_ddc_control']['traffic_bypass3']),
            self.traffic_ddc.operations_minus_one1.eq(
                self.traffic_sdr_registers['traffic_ddc_control']['traffic_operations_minus_one1']),
            self.traffic_ddc.operations_minus_one2.eq(
                self.traffic_sdr_registers['traffic_ddc_control']['traffic_operations_minus_one2']),
            self.traffic_ddc.operations_minus_one3.eq(
                self.traffic_sdr_registers['traffic_ddc_control']['traffic_operations_minus_one3']),
            self.traffic_ddc.odd_operations1.eq(
                self.traffic_sdr_registers['traffic_ddc_control']['traffic_odd_operations1']),
            self.traffic_ddc.odd_operations3.eq(
                self.traffic_sdr_registers['traffic_ddc_control']['traffic_odd_operations3']),
            self.traffic_ddc.strobe_in.eq(rxiq_cdc.strobe_out),
            self.traffic_ddc.re_in.eq(rxiq_cdc.re_out),
            self.traffic_ddc.im_in.eq(rxiq_cdc.im_out),
        ]

        # ── Traffic chain 2 DDC (core 0.3.0, doc/changes/064) ─────────
        # Same wiring as the traffic DDC, from bank 9. The input is
        # re-registered once in `sync` instead of tapping
        # `rxiq_cdc.strobe_out / re_out / im_out` directly: that net
        # already fans out to two DDCs, the wideband IQ packer and the
        # spectrometer, and the 0.2.0 worst setup path is
        # `rxiq_cdc/strobe_out_reg` -> spectrometer (sync -> clk3x,
        # +0.255 ns). One sync cycle (16 ns) of extra latency is
        # irrelevant to the chain.
        traffic2_in_re = Signal(self.iq_in_width, reset_less=True)
        traffic2_in_im = Signal(self.iq_in_width, reset_less=True)
        traffic2_in_strobe = Signal()
        m.d.sync += [
            traffic2_in_re.eq(rxiq_cdc.re_out),
            traffic2_in_im.eq(rxiq_cdc.im_out),
            traffic2_in_strobe.eq(rxiq_cdc.strobe_out),
        ]
        t2_sdr = self.traffic2_sdr_registers
        t2_ddc_ctrl = t2_sdr['traffic2_ddc_control']
        t2_ddc_dec = t2_sdr['traffic2_ddc_decimation']
        m.d.comb += [
            self.traffic2_ddc.common_edge.eq(common_edge_3x.common_edge),
            self.traffic2_ddc.enable_input.eq(
                t2_ddc_ctrl['traffic2_enable_input']),
            self.traffic2_ddc.frequency.eq(
                t2_sdr['traffic2_ddc_frequency']['traffic2_frequency']),
            self.traffic2_ddc.coeff_waddr.eq(
                t2_sdr['traffic2_ddc_coeff_addr']['traffic2_coeff_waddr']),
            self.traffic2_ddc.coeff_wren.eq(
                t2_sdr['traffic2_ddc_coeff']['traffic2_coeff_wren']),
            self.traffic2_ddc.coeff_wdata.eq(
                t2_sdr['traffic2_ddc_coeff']['traffic2_coeff_wdata']),
            self.traffic2_ddc.decimation1.eq(
                t2_ddc_dec['traffic2_decimation1']),
            self.traffic2_ddc.decimation2.eq(
                t2_ddc_dec['traffic2_decimation2']),
            self.traffic2_ddc.decimation3.eq(
                t2_ddc_dec['traffic2_decimation3']),
            self.traffic2_ddc.bypass2.eq(t2_ddc_ctrl['traffic2_bypass2']),
            self.traffic2_ddc.bypass3.eq(t2_ddc_ctrl['traffic2_bypass3']),
            self.traffic2_ddc.operations_minus_one1.eq(
                t2_ddc_ctrl['traffic2_operations_minus_one1']),
            self.traffic2_ddc.operations_minus_one2.eq(
                t2_ddc_ctrl['traffic2_operations_minus_one2']),
            self.traffic2_ddc.operations_minus_one3.eq(
                t2_ddc_ctrl['traffic2_operations_minus_one3']),
            self.traffic2_ddc.odd_operations1.eq(
                t2_ddc_ctrl['traffic2_odd_operations1']),
            self.traffic2_ddc.odd_operations3.eq(
                t2_ddc_ctrl['traffic2_odd_operations3']),
            self.traffic2_ddc.strobe_in.eq(traffic2_in_strobe),
            self.traffic2_ddc.re_in.eq(traffic2_in_re),
            self.traffic2_ddc.im_in.eq(traffic2_in_im),
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

        # 2026-05-03: wideband raw IQ tap submodules.
        m.submodules.wideband_iq_packer = self.wideband_iq_packer
        m.submodules.wideband_iq_dma = self.wideband_iq_dma
        m.submodules.wideband_iq_registers = self.wideband_iq_registers
        m.submodules.wideband_iq_registers_cdc = (
            wideband_iq_registers_cdc) = RegisterCDC(
                's_axi_lite', 'sync', self.wideband_iq_registers.aw)

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

        # 2026-05-03 dual-DDC: traffic-channel LSM demod chain submodules.
        m.submodules.traffic_lsm_decimator = self.traffic_lsm_decimator
        m.submodules.traffic_lsm_lpf = self.traffic_lsm_lpf
        m.submodules.traffic_lsm_rrc = self.traffic_lsm_rrc
        m.submodules.traffic_lsm_demod = self.traffic_lsm_demod
        m.submodules.traffic_lsm_dibit_packer = (
            self.traffic_lsm_dibit_packer)
        m.submodules.traffic_lsm_dibit_dma = self.traffic_lsm_dibit_dma
        m.submodules.traffic_lsm_registers = self.traffic_lsm_registers
        m.submodules.traffic_lsm_registers_cdc = (
            traffic_lsm_registers_cdc) = RegisterCDC(
                's_axi_lite', 'sync', self.traffic_lsm_registers.aw)

        # Core 0.3.0: traffic chain 2 LSM demod chain submodules.
        m.submodules.traffic2_lsm_decimator = self.traffic2_lsm_decimator
        m.submodules.traffic2_lsm_lpf = self.traffic2_lsm_lpf
        m.submodules.traffic2_lsm_rrc = self.traffic2_lsm_rrc
        m.submodules.traffic2_lsm_demod = self.traffic2_lsm_demod
        m.submodules.traffic2_lsm_dibit_packer = (
            self.traffic2_lsm_dibit_packer)
        m.submodules.traffic2_lsm_dibit_dma = self.traffic2_lsm_dibit_dma
        m.submodules.traffic2_lsm_registers = self.traffic2_lsm_registers
        m.submodules.traffic2_lsm_registers_cdc = (
            traffic2_lsm_registers_cdc) = RegisterCDC(
                's_axi_lite', 'sync', self.traffic2_lsm_registers.aw)

        # 2026-05-03 seeding bake: warm-start seed register bank.
        m.submodules.lsm_seed_registers = self.lsm_seed_registers
        m.submodules.lsm_seed_registers_cdc = (
            lsm_seed_registers_cdc) = RegisterCDC(
                's_axi_lite', 'sync', self.lsm_seed_registers.aw)

        # 2026-05-03 dual-DDC: traffic-side post-DDC IQ ring DMA.
        m.submodules.traffic_iq_packer = self.traffic_iq_packer
        m.submodules.traffic_iq_dma = self.traffic_iq_dma
        m.submodules.traffic_iq_registers = self.traffic_iq_registers
        m.submodules.traffic_iq_registers_cdc = (
            traffic_iq_registers_cdc) = RegisterCDC(
                's_axi_lite', 'sync', self.traffic_iq_registers.aw)

        # 2026-05-03 dual-DDC: traffic-side pre-diff IQ ring DMA.
        m.submodules.traffic_pre_diff_iq_packer = (
            self.traffic_pre_diff_iq_packer)
        m.submodules.traffic_pre_diff_iq_dma = (
            self.traffic_pre_diff_iq_dma)
        m.submodules.traffic_pre_diff_iq_registers = (
            self.traffic_pre_diff_iq_registers)
        m.submodules.traffic_pre_diff_iq_registers_cdc = (
            traffic_pre_diff_iq_registers_cdc) = RegisterCDC(
                's_axi_lite', 'sync',
                self.traffic_pre_diff_iq_registers.aw)

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
        # Phase 10.8: control-side pre-diff IQ DMA interrupt.
        m.submodules.pre_diff_iq_dma_irq_sync = (
            pre_diff_iq_dma_irq_sync) = (
                PulseSynchronizer('sync', 's_axi_lite'))
        # Phase 10.7: wideband spectrometer.
        m.submodules.wideband_spec_dma_irq_sync = (
            wideband_spec_dma_irq_sync) = (
                PulseSynchronizer('sync', 's_axi_lite'))
        # M2B 2026-05-02: traffic LSM dibit DMA interrupt.
        m.submodules.traffic_lsm_dibit_dma_irq_sync = (
            traffic_lsm_dibit_dma_irq_sync) = (
                PulseSynchronizer('sync', 's_axi_lite'))
        # 2026-05-03 dual-DDC: traffic post-DDC IQ DMA interrupt.
        m.submodules.traffic_iq_dma_irq_sync = (
            traffic_iq_dma_irq_sync) = (
                PulseSynchronizer('sync', 's_axi_lite'))
        # 2026-05-03 dual-DDC: traffic pre-diff IQ DMA interrupt.
        m.submodules.traffic_pre_diff_iq_dma_irq_sync = (
            traffic_pre_diff_iq_dma_irq_sync) = (
                PulseSynchronizer('sync', 's_axi_lite'))
        # 2026-05-03: wideband raw IQ DMA interrupt.
        m.submodules.wideband_iq_dma_irq_sync = (
            wideband_iq_dma_irq_sync) = (
                PulseSynchronizer('sync', 's_axi_lite'))
        # Core 0.3.0: traffic chain 2 LSM dibit DMA interrupt (bit 8).
        m.submodules.traffic2_lsm_dibit_dma_irq_sync = (
            traffic2_lsm_dibit_dma_irq_sync) = (
                PulseSynchronizer('sync', 's_axi_lite'))

        m.d.comb += [
            iq_dma_irq_sync.i.eq(self.iq_dma.interrupt),
            lsm_dibit_dma_irq_sync.i.eq(self.lsm_dibit_dma.interrupt),
            pre_diff_iq_dma_irq_sync.i.eq(
                self.pre_diff_iq_dma.interrupt),
            wideband_spec_dma_irq_sync.i.eq(
                self.wideband_spec.interrupt_out),
            traffic_lsm_dibit_dma_irq_sync.i.eq(
                self.traffic_lsm_dibit_dma.interrupt),
            traffic_iq_dma_irq_sync.i.eq(
                self.traffic_iq_dma.interrupt),
            traffic_pre_diff_iq_dma_irq_sync.i.eq(
                self.traffic_pre_diff_iq_dma.interrupt),
            wideband_iq_dma_irq_sync.i.eq(
                self.wideband_iq_dma.interrupt),
            traffic2_lsm_dibit_dma_irq_sync.i.eq(
                self.traffic2_lsm_dibit_dma.interrupt),
            # Feed the synchronized pulses into the Rsticky bits.
            interrupts_reg['pre_diff_iq_dma'].eq(
                pre_diff_iq_dma_irq_sync.o),
            interrupts_reg['wideband_spec_dma'].eq(
                wideband_spec_dma_irq_sync.o),
            interrupts_reg['traffic_lsm_dibit_dma'].eq(
                traffic_lsm_dibit_dma_irq_sync.o),
            interrupts_reg['traffic_iq_dma'].eq(
                traffic_iq_dma_irq_sync.o),
            interrupts_reg['traffic_pre_diff_iq_dma'].eq(
                traffic_pre_diff_iq_dma_irq_sync.o),
            interrupts_reg['wideband_iq_dma'].eq(
                wideband_iq_dma_irq_sync.o),
            interrupts_reg['traffic2_lsm_dibit_dma'].eq(
                traffic2_lsm_dibit_dma_irq_sync.o),
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

        # ── Wideband raw IQ ring DMA (2026-05-03) ─────────────────────
        # Pre-DDC tap of the rxiq_cdc output. `rxiq_cdc.re_out/im_out`
        # are 12-bit unsigned wrappers of 12-bit signed two's complement
        # — `as_signed()` reinterprets the bits, then assignment to the
        # packer's signed(16) inputs sign-extends correctly. Without
        # the `as_signed()` we get unsigned 0..4095 → DC offset of
        # +2048 and apparent rails at +4095 in the captured stream.
        # (Bake #2 shipped without this; caught via tools/p25_iq_inspect.py.)
        # Same fix is already applied to the wideband spectrometer tap.
        m.d.comb += [
            self.wideband_iq_packer.re_in.eq(rxiq_cdc.re_out.as_signed()),
            self.wideband_iq_packer.im_in.eq(rxiq_cdc.im_out.as_signed()),
            self.wideband_iq_packer.strobe_in.eq(rxiq_cdc.strobe_out),
            self.wideband_iq_dma.stream_data.eq(
                self.wideband_iq_packer.data_out),
            self.wideband_iq_dma.stream_valid.eq(
                self.wideband_iq_packer.data_valid),
            self.wideband_iq_packer.stream_ready.eq(
                self.wideband_iq_dma.stream_ready),
            self.wideband_iq_dma.enable.eq(
                self.wideband_iq_registers[
                    'wideband_iq_dma_control']['wideband_iq_enable']),
            self.wideband_iq_registers[
                'wideband_iq_dma_status']['wideband_iq_overflow'].eq(
                self.wideband_iq_packer.overflow),
            self.wideband_iq_registers[
                'wideband_iq_dma_status']['last_buffer'].eq(
                self.wideband_iq_dma.last_buffer),
            self.wideband_iq_registers[
                'wideband_iq_next_address']['next_address'].eq(
                self.wideband_iq_dma.axi.awaddr),
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
            # 2026-05-03 seeding bake: warm-start seeds for the
            # control chain. Latched into the AGC/PLL/timing
            # accumulators on the same `lsm_reset` pulse. Zero ->
            # cold-start init (legacy behaviour).
            self.lsm_demod.agc_seed_in.eq(
                self.lsm_seed_registers['lsm_agc_seed']['agc_seed']),
            self.lsm_demod.pll_seed_in.eq(
                self.lsm_seed_registers['lsm_pll_seed']['pll_seed']),
            self.lsm_demod.timing_seed_in.eq(
                self.lsm_seed_registers[
                    'lsm_timing_seed']['timing_seed']),
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

        # ── Traffic LSM demod chain (dual-DDC pivot, 2026-05-03) ──
        # Mirror of control LSM chain elaboration order. Pipeline:
        #   traffic_ddc out (50 kSPS post-2026-05-03)
        #     -> LsmDecimator2 /2     (-> 25 kSPS)
        #     -> LsmFir(LPF_TAPS_25K) (121-tap baseband LPF)
        #     -> LsmFir(RRC_TAPS_25K) (42-tap SDRTrunk-exact RRC)
        #     -> LsmDemod(sample_rate_hz=25_000)
        #     -> { dibit_packer -> traffic_lsm_dibit_dma,
        #          NID event registers }
        #
        # Master enable: traffic_lsm_control.traffic_lsm_enable gates
        # the strobe at the front of the decimator, so when 0 every
        # downstream block sees no strobes and goes quiescent (no PLL
        # drift, no BCH sweeps, no spurious dibits).
        traffic_lsm_ctrl = self.traffic_lsm_registers[
            'traffic_lsm_control']
        traffic_lsm_stat = self.traffic_lsm_registers[
            'traffic_lsm_status']
        traffic_lsm_nid_reg = self.traffic_lsm_registers[
            'traffic_lsm_nid']
        traffic_lsm_drop_reg = self.traffic_lsm_registers[
            'traffic_lsm_drop_count']
        traffic_lsm_dibit_next_reg = self.traffic_lsm_registers[
            'traffic_lsm_dibit_next']
        traffic_lsm_dbg_reg = self.traffic_lsm_registers[
            'traffic_lsm_debug']
        traffic_lsm_agc_dbg_reg = self.traffic_lsm_registers[
            'traffic_lsm_agc_debug']
        traffic_lsm_agc_cfg = self.traffic_lsm_registers[
            'traffic_lsm_agc_config']

        traffic_lsm_enable_q = traffic_lsm_ctrl['traffic_lsm_enable']

        # Stage 1a: traffic DDC out (50 kSPS) -> LsmDecimator2 /2
        m.d.comb += [
            self.traffic_lsm_decimator.re_in.eq(self.traffic_ddc.re_out),
            self.traffic_lsm_decimator.im_in.eq(self.traffic_ddc.im_out),
            self.traffic_lsm_decimator.strobe_in.eq(
                self.traffic_ddc.strobe_out & traffic_lsm_enable_q),
        ]
        # Stage 1b: decimator (25 kSPS) -> LPF
        m.d.comb += [
            self.traffic_lsm_lpf.re_in.eq(self.traffic_lsm_decimator.re_out),
            self.traffic_lsm_lpf.im_in.eq(self.traffic_lsm_decimator.im_out),
            self.traffic_lsm_lpf.strobe_in.eq(
                self.traffic_lsm_decimator.strobe_out),
        ]
        # Stage 1c: LPF -> RRC
        m.d.comb += [
            self.traffic_lsm_rrc.re_in.eq(self.traffic_lsm_lpf.re_out),
            self.traffic_lsm_rrc.im_in.eq(self.traffic_lsm_lpf.im_out),
            self.traffic_lsm_rrc.strobe_in.eq(
                self.traffic_lsm_lpf.strobe_out),
        ]
        # Stage 2: RRC -> LsmDemod
        m.d.comb += [
            self.traffic_lsm_demod.re_in.eq(self.traffic_lsm_rrc.re_out),
            self.traffic_lsm_demod.im_in.eq(self.traffic_lsm_rrc.im_out),
            self.traffic_lsm_demod.strobe_in.eq(
                self.traffic_lsm_rrc.strobe_out),
            self.traffic_lsm_demod.dc_block_enable.eq(
                traffic_lsm_ctrl['traffic_lsm_dc_block_enable']),
            self.traffic_lsm_demod.agc_enable.eq(
                traffic_lsm_ctrl['traffic_lsm_agc_enable']),
            self.traffic_lsm_demod.reset_in.eq(
                traffic_lsm_ctrl['traffic_lsm_reset']),
            self.traffic_lsm_demod.agc_mag_update_threshold_in.eq(
                traffic_lsm_agc_cfg['mag_update_threshold']),
            # 2026-05-03 seeding bake: warm-start seeds for the
            # traffic chain. PS copies converged values from the
            # control chain (or per-freq cache) into these
            # registers before pulsing `traffic_lsm_reset` on a
            # retune. Zero -> cold start.
            self.traffic_lsm_demod.agc_seed_in.eq(
                self.lsm_seed_registers[
                    'traffic_lsm_agc_seed']['agc_seed']),
            self.traffic_lsm_demod.pll_seed_in.eq(
                self.lsm_seed_registers[
                    'traffic_lsm_pll_seed']['pll_seed']),
            self.traffic_lsm_demod.timing_seed_in.eq(
                self.lsm_seed_registers[
                    'traffic_lsm_timing_seed']['timing_seed']),
        ]
        # Stage 3: LsmDemod dibits -> packer -> ring DMA stream
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
                traffic_lsm_ctrl['traffic_lsm_dibit_dma_enable']),
        ]

        # Stage 4: NID event latching — mirror of the control chain.
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
                traffic_latched_valid.eq(
                    self.traffic_lsm_demod.valid_out),
                traffic_latched_sync_distance.eq(
                    self.traffic_lsm_demod.sync_distance_out),
            ]

        # Status / NID event surfacing into the register bank.
        m.d.comb += [
            traffic_lsm_stat['bch_busy'].eq(
                self.traffic_lsm_demod.bch_busy),
            traffic_lsm_stat['in_nid_window'].eq(
                self.traffic_lsm_demod.in_nid_window),
            traffic_lsm_stat['nid_event'].eq(
                self.traffic_lsm_demod.nid_event_strobe),
            traffic_lsm_stat['nid_valid'].eq(traffic_latched_valid),
            traffic_lsm_stat['n_errors'].eq(traffic_latched_n_errors),
            traffic_lsm_stat['sync_distance'].eq(
                traffic_latched_sync_distance),
            traffic_lsm_stat['traffic_lsm_dibit_overflow'].eq(
                self.traffic_lsm_dibit_packer.overflow),
            traffic_lsm_nid_reg['nac'].eq(traffic_latched_nac),
            traffic_lsm_nid_reg['duid'].eq(traffic_latched_duid),
            traffic_lsm_drop_reg['drop_count'].eq(
                self.traffic_lsm_demod.nid_drop_count),
            traffic_lsm_drop_reg[
                'traffic_lsm_dibit_last_buffer'].eq(
                self.traffic_lsm_dibit_dma.last_buffer),
            traffic_lsm_dibit_next_reg['next_address'].eq(
                self.traffic_lsm_dibit_dma.axi.awaddr),
            traffic_lsm_dbg_reg['pll_dbg'].eq(
                self.traffic_lsm_demod.pll_dbg),
            traffic_lsm_dbg_reg['sample_point_dbg'].eq(
                self.traffic_lsm_demod.sample_point_dbg[2:]),
            traffic_lsm_agc_dbg_reg['agc_gain_dbg'].eq(
                self.traffic_lsm_demod.agc_gain_dbg),
            traffic_lsm_agc_dbg_reg['agc_mag_dbg'].eq(
                self.traffic_lsm_demod.agc_mag_dbg),
        ]

        # ── Traffic chain 2 LSM demod chain (core 0.3.0, 064) ──────────
        # Same wiring as the traffic chain above; registers and seeds
        # come from the 16-word `traffic2_lsm` bank (0x140). The master
        # enable gates the strobe at the decimator, so with
        # traffic2_lsm_enable = 0 (reset value) the whole chain idles
        # and its DMA never raises an interrupt: a 0.2.0-era PS that
        # does not know chain 2 sees no change in behaviour.
        t2_lsm = self.traffic2_lsm_registers
        t2_ctrl = t2_lsm['traffic2_lsm_control']
        t2_stat = t2_lsm['traffic2_lsm_status']
        t2_nid = t2_lsm['traffic2_lsm_nid']
        t2_drop = t2_lsm['traffic2_lsm_drop_count']
        t2_dbg = t2_lsm['traffic2_lsm_debug']
        t2_agc_dbg = t2_lsm['traffic2_lsm_agc_debug']
        t2_agc_cfg = t2_lsm['traffic2_lsm_agc_config']
        t2_demod = self.traffic2_lsm_demod

        m.d.comb += [
            # traffic2_ddc (50 kSPS) -> LsmDecimator2 /2 (25 kSPS)
            self.traffic2_lsm_decimator.re_in.eq(self.traffic2_ddc.re_out),
            self.traffic2_lsm_decimator.im_in.eq(self.traffic2_ddc.im_out),
            self.traffic2_lsm_decimator.strobe_in.eq(
                self.traffic2_ddc.strobe_out
                & t2_ctrl['traffic2_lsm_enable']),
            # decimator -> LPF
            self.traffic2_lsm_lpf.re_in.eq(self.traffic2_lsm_decimator.re_out),
            self.traffic2_lsm_lpf.im_in.eq(self.traffic2_lsm_decimator.im_out),
            self.traffic2_lsm_lpf.strobe_in.eq(
                self.traffic2_lsm_decimator.strobe_out),
            # LPF -> RRC
            self.traffic2_lsm_rrc.re_in.eq(self.traffic2_lsm_lpf.re_out),
            self.traffic2_lsm_rrc.im_in.eq(self.traffic2_lsm_lpf.im_out),
            self.traffic2_lsm_rrc.strobe_in.eq(
                self.traffic2_lsm_lpf.strobe_out),
            # RRC -> LsmDemod
            t2_demod.re_in.eq(self.traffic2_lsm_rrc.re_out),
            t2_demod.im_in.eq(self.traffic2_lsm_rrc.im_out),
            t2_demod.strobe_in.eq(self.traffic2_lsm_rrc.strobe_out),
            t2_demod.dc_block_enable.eq(
                t2_ctrl['traffic2_lsm_dc_block_enable']),
            t2_demod.agc_enable.eq(t2_ctrl['traffic2_lsm_agc_enable']),
            t2_demod.reset_in.eq(t2_ctrl['traffic2_lsm_reset']),
            t2_demod.agc_mag_update_threshold_in.eq(
                t2_agc_cfg['mag_update_threshold']),
            # Warm-start seeds, latched on traffic2_lsm_reset.
            t2_demod.agc_seed_in.eq(
                t2_lsm['traffic2_lsm_agc_seed']['agc_seed']),
            t2_demod.pll_seed_in.eq(
                t2_lsm['traffic2_lsm_pll_seed']['pll_seed']),
            t2_demod.timing_seed_in.eq(
                t2_lsm['traffic2_lsm_timing_seed']['timing_seed']),
            # LsmDemod dibits -> packer -> ring DMA stream
            self.traffic2_lsm_dibit_packer.dibit_in.eq(t2_demod.dibit_out),
            self.traffic2_lsm_dibit_packer.symbol_strobe.eq(
                t2_demod.symbol_strobe),
            self.traffic2_lsm_dibit_dma.stream_data.eq(
                self.traffic2_lsm_dibit_packer.data_out),
            self.traffic2_lsm_dibit_dma.stream_valid.eq(
                self.traffic2_lsm_dibit_packer.data_valid),
            self.traffic2_lsm_dibit_packer.stream_ready.eq(
                self.traffic2_lsm_dibit_dma.stream_ready),
            self.traffic2_lsm_dibit_dma.enable.eq(
                t2_ctrl['traffic2_lsm_dibit_dma_enable']),
        ]

        # NID event latching — mirror of the traffic chain.
        traffic2_latched_nac = Signal(12, reset_less=True)
        traffic2_latched_duid = Signal(4, reset_less=True)
        traffic2_latched_n_errors = Signal(7, reset_less=True)
        traffic2_latched_valid = Signal(reset_less=True)
        traffic2_latched_sync_distance = Signal(7, reset_less=True)
        with m.If(t2_demod.nid_event_strobe):
            m.d.sync += [
                traffic2_latched_nac.eq(t2_demod.nac_out),
                traffic2_latched_duid.eq(t2_demod.duid_out),
                traffic2_latched_n_errors.eq(t2_demod.n_errors_out),
                traffic2_latched_valid.eq(t2_demod.valid_out),
                traffic2_latched_sync_distance.eq(
                    t2_demod.sync_distance_out),
            ]

        m.d.comb += [
            t2_stat['bch_busy'].eq(t2_demod.bch_busy),
            t2_stat['in_nid_window'].eq(t2_demod.in_nid_window),
            t2_stat['nid_event'].eq(t2_demod.nid_event_strobe),
            t2_stat['nid_valid'].eq(traffic2_latched_valid),
            t2_stat['n_errors'].eq(traffic2_latched_n_errors),
            t2_stat['sync_distance'].eq(traffic2_latched_sync_distance),
            t2_stat['traffic2_lsm_dibit_overflow'].eq(
                self.traffic2_lsm_dibit_packer.overflow),
            t2_nid['nac'].eq(traffic2_latched_nac),
            t2_nid['duid'].eq(traffic2_latched_duid),
            t2_drop['drop_count'].eq(t2_demod.nid_drop_count),
            t2_drop['traffic2_lsm_dibit_last_buffer'].eq(
                self.traffic2_lsm_dibit_dma.last_buffer),
            t2_lsm['traffic2_lsm_dibit_next']['next_address'].eq(
                self.traffic2_lsm_dibit_dma.axi.awaddr),
            t2_dbg['pll_dbg'].eq(t2_demod.pll_dbg),
            t2_dbg['sample_point_dbg'].eq(t2_demod.sample_point_dbg[2:]),
            t2_agc_dbg['agc_gain_dbg'].eq(t2_demod.agc_gain_dbg),
            t2_agc_dbg['agc_mag_dbg'].eq(t2_demod.agc_mag_dbg),
        ]

        # ── Traffic-side post-DDC IQ ring DMA (2026-05-03) ────────────
        # Mirror of the control `iq_packer` + `iq_dma`. Tap is
        # `traffic_ddc.re_out / im_out / strobe_out` at the DDC output
        # rate (50 kSPS post-2026-05-03 retune).
        m.d.comb += [
            self.traffic_iq_packer.re_in.eq(self.traffic_ddc.re_out),
            self.traffic_iq_packer.im_in.eq(self.traffic_ddc.im_out),
            self.traffic_iq_packer.strobe_in.eq(
                self.traffic_ddc.strobe_out),
            self.traffic_iq_dma.stream_data.eq(
                self.traffic_iq_packer.data_out),
            self.traffic_iq_dma.stream_valid.eq(
                self.traffic_iq_packer.data_valid),
            self.traffic_iq_packer.stream_ready.eq(
                self.traffic_iq_dma.stream_ready),
            self.traffic_iq_dma.enable.eq(
                self.traffic_iq_registers[
                    'traffic_iq_dma_control']['traffic_iq_enable']),
            self.traffic_iq_registers[
                'traffic_iq_dma_status']['traffic_iq_overflow'].eq(
                self.traffic_iq_packer.overflow),
            self.traffic_iq_registers[
                'traffic_iq_dma_status']['traffic_iq_last_buffer'].eq(
                self.traffic_iq_dma.last_buffer),
            self.traffic_iq_registers[
                'traffic_iq_next_address']['next_address'].eq(
                self.traffic_iq_dma.axi.awaddr),
        ]

        # ── Traffic-side pre-diff IQ ring DMA (2026-05-03) ────────────
        # Mirror of the control `pre_diff_iq_packer` + `pre_diff_iq_dma`.
        # Tap is `traffic_lsm_demod.{i,q}_pre_diff_out / pre_diff_strobe_out`
        # — AGC-scaled, carrier-derotated, mid/sym interleaved on
        # adjacent `pre_diff_strobe_out` cycles.
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
                'traffic_pre_diff_iq_dma_status'][
                'traffic_pre_diff_iq_last_buffer'].eq(
                self.traffic_pre_diff_iq_dma.last_buffer),
            self.traffic_pre_diff_iq_registers[
                'traffic_pre_diff_iq_next_address'][
                'next_address'].eq(
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
        # 2026-05-03 dual-DDC layout:
        #   word 0x00-0x07: control                 (bank 0)
        #   word 0x08-0x0F: sdr (control DDC)       (bank 1)
        #   word 0x10-0x17: traffic_sdr             (bank 2)
        #   word 0x18-0x1F: traffic_iq              (bank 3)
        #   word 0x20-0x27: iq (control)            (bank 4)
        #   word 0x28-0x2F: lsm (control)           (bank 5)
        #   word 0x30-0x37: traffic_lsm             (bank 6)
        #   word 0x38-0x3F: wideband_iq             (bank 7)
        #   word 0x40-0x47: lsm_seed                (bank 8)
        #   word 0x48-0x4F: traffic2_sdr            (bank 9, 0.3.0)
        #   word 0x50-0x5F: traffic2_lsm            (banks 10-11, 0.3.0,
        #                                            16-word bank)
        #   word 0x60-0x67: spectrometer            (bank 12)
        #   word 0x68-0x6F: pre_diff_iq (control)   (bank 13)
        #   word 0x70-0x77: traffic_pre_diff_iq     (bank 14)
        # Bank 15 is vacant.
        address = Signal(self.axi4_awidth, reset_less=True)
        wdata = Signal(32, reset_less=True)
        addr_bank = self.axi4lite.address[3:7]  # bits [6:3]
        control_regs_select = (addr_bank == 0b0000)
        sdr_regs_select = (addr_bank == 0b0001)
        traffic_sdr_regs_select = (addr_bank == 0b0010)
        traffic_iq_regs_select = (addr_bank == 0b0011)
        iq_regs_select = (addr_bank == 0b0100)
        lsm_regs_select = (addr_bank == 0b0101)
        traffic_lsm_regs_select = (addr_bank == 0b0110)
        wideband_iq_regs_select = (addr_bank == 0b0111)
        lsm_seed_regs_select = (addr_bank == 0b1000)
        traffic2_sdr_regs_select = (addr_bank == 0b1001)
        # 16-word bank: banks 10 and 11 (bits [6:4] == 0b101).
        traffic2_lsm_regs_select = (addr_bank[1:] == 0b101)
        spec_regs_select = (addr_bank == 0b1100)
        pre_diff_iq_regs_select = (addr_bank == 0b1101)
        traffic_pre_diff_iq_regs_select = (addr_bank == 0b1110)

        m.d.s_axi_lite += [
            self.axi4lite.rdata.eq(self.control_registers.rdata
                                   | sdr_registers_cdc.i_rdata
                                   | traffic_sdr_registers_cdc.i_rdata
                                   | traffic_iq_registers_cdc.i_rdata
                                   | iq_registers_cdc.i_rdata
                                   | lsm_registers_cdc.i_rdata
                                   | traffic_lsm_registers_cdc.i_rdata
                                   | wideband_iq_registers_cdc.i_rdata
                                   | lsm_seed_registers_cdc.i_rdata
                                   | traffic2_sdr_registers_cdc.i_rdata
                                   | traffic2_lsm_registers_cdc.i_rdata
                                   | pre_diff_iq_registers_cdc.i_rdata
                                   | traffic_pre_diff_iq_registers_cdc.i_rdata
                                   | wideband_spec_registers_cdc.i_rdata),
            self.axi4lite.rdone.eq(self.control_registers.rdone
                                   | sdr_registers_cdc.i_rdone
                                   | traffic_sdr_registers_cdc.i_rdone
                                   | traffic_iq_registers_cdc.i_rdone
                                   | iq_registers_cdc.i_rdone
                                   | lsm_registers_cdc.i_rdone
                                   | traffic_lsm_registers_cdc.i_rdone
                                   | wideband_iq_registers_cdc.i_rdone
                                   | lsm_seed_registers_cdc.i_rdone
                                   | traffic2_sdr_registers_cdc.i_rdone
                                   | traffic2_lsm_registers_cdc.i_rdone
                                   | pre_diff_iq_registers_cdc.i_rdone
                                   | traffic_pre_diff_iq_registers_cdc.i_rdone
                                   | wideband_spec_registers_cdc.i_rdone),
            self.axi4lite.wdone.eq(self.control_registers.wdone
                                   | sdr_registers_cdc.i_wdone
                                   | traffic_sdr_registers_cdc.i_wdone
                                   | traffic_iq_registers_cdc.i_wdone
                                   | iq_registers_cdc.i_wdone
                                   | lsm_registers_cdc.i_wdone
                                   | traffic_lsm_registers_cdc.i_wdone
                                   | wideband_iq_registers_cdc.i_wdone
                                   | lsm_seed_registers_cdc.i_wdone
                                   | traffic2_sdr_registers_cdc.i_wdone
                                   | traffic2_lsm_registers_cdc.i_wdone
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
            traffic_sdr_registers_cdc.i_ren.eq(
                self.axi4lite.ren & traffic_sdr_regs_select),
            traffic_sdr_registers_cdc.i_wstrobe.eq(
                Mux(traffic_sdr_regs_select, self.axi4lite.wstrobe, 0)),
            traffic_iq_registers_cdc.i_ren.eq(
                self.axi4lite.ren & traffic_iq_regs_select),
            traffic_iq_registers_cdc.i_wstrobe.eq(
                Mux(traffic_iq_regs_select, self.axi4lite.wstrobe, 0)),
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
            wideband_iq_registers_cdc.i_ren.eq(
                self.axi4lite.ren & wideband_iq_regs_select),
            wideband_iq_registers_cdc.i_wstrobe.eq(
                Mux(wideband_iq_regs_select, self.axi4lite.wstrobe, 0)),
            lsm_seed_registers_cdc.i_ren.eq(
                self.axi4lite.ren & lsm_seed_regs_select),
            lsm_seed_registers_cdc.i_wstrobe.eq(
                Mux(lsm_seed_regs_select, self.axi4lite.wstrobe, 0)),
            traffic2_sdr_registers_cdc.i_ren.eq(
                self.axi4lite.ren & traffic2_sdr_regs_select),
            traffic2_sdr_registers_cdc.i_wstrobe.eq(
                Mux(traffic2_sdr_regs_select, self.axi4lite.wstrobe, 0)),
            traffic2_lsm_registers_cdc.i_ren.eq(
                self.axi4lite.ren & traffic2_lsm_regs_select),
            traffic2_lsm_registers_cdc.i_wstrobe.eq(
                Mux(traffic2_lsm_regs_select, self.axi4lite.wstrobe, 0)),
            wideband_spec_registers_cdc.i_ren.eq(
                self.axi4lite.ren & spec_regs_select),
            wideband_spec_registers_cdc.i_wstrobe.eq(
                Mux(spec_regs_select, self.axi4lite.wstrobe, 0)),
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
            traffic_sdr_registers_cdc.i_address.eq(address),
            traffic_sdr_registers_cdc.i_wdata.eq(wdata),
            traffic_iq_registers_cdc.i_address.eq(address),
            traffic_iq_registers_cdc.i_wdata.eq(wdata),
            iq_registers_cdc.i_address.eq(address),
            iq_registers_cdc.i_wdata.eq(wdata),
            lsm_registers_cdc.i_address.eq(address),
            lsm_registers_cdc.i_wdata.eq(wdata),
            traffic_lsm_registers_cdc.i_address.eq(address),
            traffic_lsm_registers_cdc.i_wdata.eq(wdata),
            wideband_iq_registers_cdc.i_address.eq(address),
            wideband_iq_registers_cdc.i_wdata.eq(wdata),
            lsm_seed_registers_cdc.i_address.eq(address),
            lsm_seed_registers_cdc.i_wdata.eq(wdata),
            traffic2_sdr_registers_cdc.i_address.eq(address),
            traffic2_sdr_registers_cdc.i_wdata.eq(wdata),
            traffic2_lsm_registers_cdc.i_address.eq(address),
            traffic2_lsm_registers_cdc.i_wdata.eq(wdata),
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
        # traffic_lsm_registers CDC (M2B 2026-05-02)
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
        # 2026-05-03: lsm_seed_registers CDC (seeding bake bank 8).
        m.d.comb += [
            self.lsm_seed_registers.ren.eq(
                lsm_seed_registers_cdc.o_ren),
            self.lsm_seed_registers.wstrobe.eq(
                lsm_seed_registers_cdc.o_wstrobe),
            self.lsm_seed_registers.address.eq(
                lsm_seed_registers_cdc.o_address),
            self.lsm_seed_registers.wdata.eq(
                lsm_seed_registers_cdc.o_wdata),
            lsm_seed_registers_cdc.o_rdone.eq(
                self.lsm_seed_registers.rdone),
            lsm_seed_registers_cdc.o_wdone.eq(
                self.lsm_seed_registers.wdone),
            lsm_seed_registers_cdc.o_rdata.eq(
                self.lsm_seed_registers.rdata),
        ]
        # Phase 10.8: pre_diff_iq register CDC.
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
        # 2026-05-03: wideband_iq register CDC.
        m.d.comb += [
            self.wideband_iq_registers.ren.eq(
                wideband_iq_registers_cdc.o_ren),
            self.wideband_iq_registers.wstrobe.eq(
                wideband_iq_registers_cdc.o_wstrobe),
            self.wideband_iq_registers.address.eq(
                wideband_iq_registers_cdc.o_address),
            self.wideband_iq_registers.wdata.eq(
                wideband_iq_registers_cdc.o_wdata),
            wideband_iq_registers_cdc.o_rdone.eq(
                self.wideband_iq_registers.rdone),
            wideband_iq_registers_cdc.o_wdone.eq(
                self.wideband_iq_registers.wdone),
            wideband_iq_registers_cdc.o_rdata.eq(
                self.wideband_iq_registers.rdata),
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
        # 2026-05-03 dual-DDC pivot: traffic_sdr register CDC
        # (replaces the retired traffic_pipe register CDC block).
        m.d.comb += [
            self.traffic_sdr_registers.ren.eq(
                traffic_sdr_registers_cdc.o_ren),
            self.traffic_sdr_registers.wstrobe.eq(
                traffic_sdr_registers_cdc.o_wstrobe),
            self.traffic_sdr_registers.address.eq(
                traffic_sdr_registers_cdc.o_address),
            self.traffic_sdr_registers.wdata.eq(
                traffic_sdr_registers_cdc.o_wdata),
            traffic_sdr_registers_cdc.o_rdone.eq(
                self.traffic_sdr_registers.rdone),
            traffic_sdr_registers_cdc.o_wdone.eq(
                self.traffic_sdr_registers.wdone),
            traffic_sdr_registers_cdc.o_rdata.eq(
                self.traffic_sdr_registers.rdata),
        ]
        # 2026-05-03 dual-DDC: traffic_iq register CDC.
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
        # 2026-05-03 dual-DDC: traffic_pre_diff_iq register CDC.
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
        # Core 0.3.0: traffic chain 2 register CDCs (banks 9, 10-11).
        for regs, cdc in [
                (self.traffic2_sdr_registers, traffic2_sdr_registers_cdc),
                (self.traffic2_lsm_registers, traffic2_lsm_registers_cdc)]:
            m.d.comb += [
                regs.ren.eq(cdc.o_ren),
                regs.wstrobe.eq(cdc.o_wstrobe),
                regs.address.eq(cdc.o_address),
                regs.wdata.eq(cdc.o_wdata),
                cdc.o_rdone.eq(regs.rdone),
                cdc.o_wdone.eq(regs.wdone),
                cdc.o_rdata.eq(regs.rdata),
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
