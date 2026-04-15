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
# CURRENT STATUS (Phase 7A.2, 2026-04-11): BOTH the control channel
# AND the traffic channel run the C4FM and LSM demod chains in
# parallel. Each side has its own DDC + parallel C4FM and LSM
# pipelines feeding independent ring DMAs:
#
#   Control side:
#     ddc -> c4fm_demod  -> dibit_packer     -> dibit_dma     (0x1700_0000)
#         \-> lsm chain  -> lsm_dibit_packer -> lsm_dibit_dma (0x1A00_0000)
#         \-> iq_packer  ----------------------> iq_dma       (0x1900_0000)
#
#   Traffic side:
#     traffic_ddc -> traffic_c4fm     -> traffic_packer         -> traffic_dma         (0x1800_0000)
#                 \-> traffic_lsm chain -> traffic_lsm_dibit_packer -> traffic_lsm_dibit_dma (0x1B00_0000)
#
# Recovered NIDs from each LSM chain are surfaced via separate AXI
# register banks: `lsm` at 0xA0 (control side, bank 5, Phase 6E.9)
# and `traffic_lsm` at 0xC0 (traffic side, bank 6, Phase 7A.2). The
# PS dispatches DUID events for HDU/TDU/LDU on the traffic side
# the same way it dispatches NID events on the control side.
#
# LSM chain pipeline (added in 6E.9):
#
#   control DDC out (62.5 kSPS)
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
#   LsmDemod                          -- timing recovery + diff demod
#       |                                + Costas-style PLL rotate +
#       |                                slicer + sync detect + BCH
#       |
#       +--> dibit_out / symbol_strobe -> lsm_dibit_packer -> lsm_dibit_dma
#       |
#       +--> nid_event_strobe + (NAC, DUID, n_errors, ...) -> `lsm` register bank
#
# The LSM chain is master-enabled by `lsm.lsm_control.lsm_enable`, which
# gates the strobe at the very front of LsmDecimator2 so all downstream
# blocks go quiescent when 0. The lsm_dibit_dma ring is independently
# enabled by `lsm.lsm_control.lsm_dibit_dma_enable` (mirroring the
# dibit_dma / iq_dma pattern).
#
# C4FM chain detail (unchanged from Phase 6E.0)
# ---------------------------------------------
# The C4FM chain runs the same symbol-rate differential slicer it has
# since Phase 4: raw post-DDC IQ feeds SymbolTimingRecovery's symbol-
# rate `z[k]*conj(z[k-1])` slicer (4 DSP48E1), and `C4FMDemod.diff_im`
# (the FM cross-product) drives Gardner TED for clock recovery. The
# slicer decodes C4FM cleanly but produces ~random dibits on LSM
# (empirically confirmed against Clay County NAC 0x8A1, 2026-04-09),
# which is exactly why the LSM chain above exists in parallel.
#
# SPDX-License-Identifier: MIT
#

import argparse
import sys
import os

from amaranth import *
from amaranth.lib.cdc import FFSynchronizer
import amaranth.back.verilog

from maia_hdl.axi4_lite import Axi4LiteRegisterBridge
from maia_hdl.cdc import RegisterCDC, RxIQCDC
from maia_hdl.clknx import ClkNxCommonEdge
from maia_hdl.ddc import DDC
from maia_hdl.dma import DmaStreamRingWrite
from maia_hdl.pluto_platform import PlutoPlatform
from maia_hdl.register import Access, Field, Registers, Register, RegisterMap

from .c4fm_demod import C4FMDemod
from .symbol_timing import SymbolTimingRecovery
from .dibit_packer import DibitPacker
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
      AD9361 IQ -> DDC (tune + decimate) -> C4FM demod -> symbol timing
      -> dibit packer -> DMA to PS

    Reuses Maia SDR DDC, register infrastructure, and DMA modules.
    """
    def __init__(self, config=P25Config()):
        config.validate()
        self.config = config
        self.axi4_awidth = 7  # 7 bits = 128 registers (control, recorder, sdr, demod, traffic)
        self.s_axi_lite = ClockDomain()
        self.sampling = ClockDomain()
        self.sync = ClockDomain()
        self.clk3x = ClockDomain()

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
                    Field('dibit_dma', Access.Rsticky, 1, 0),
                    Field('traffic_dma', Access.Rsticky, 1, 0),
                    # Phase 6C: control-channel post-DDC IQ ring DMA
                    Field('iq_dma', Access.Rsticky, 1, 0),
                    # Phase 6E.9: control-channel LSM dibit ring DMA
                    Field('lsm_dibit_dma', Access.Rsticky, 1, 0),
                    # Phase 7A.2: traffic-channel LSM dibit ring DMA
                    Field('traffic_lsm_dibit_dma', Access.Rsticky, 1, 0),
                ], interrupt=True),
            },
            2)

        # ── Control DDC registers (0x08) ──────────────────────────────
        self.ddc = DDC('clk3x')

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

        # ── Control channel demod chain ───────────────────────────────
        self.c4fm_demod = C4FMDemod()
        # 8 MSPS ADC / 128x DDC decimation = 62.5 kSPS / 4800 sym/s ≈ 13 samp/sym
        self.symbol_timing = SymbolTimingRecovery(samples_per_symbol=13)
        self.dibit_packer = DibitPacker()
        # Continuous ring DMA: fires interrupt on each sub-buffer completion
        self.dibit_dma = DmaStreamRingWrite(
            config.dibit_dma_address,
            config.dibit_dma_num_buffers_log2,
            config.dibit_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_dibit')

        # ── Control channel demod registers (0x40) ────────────────────
        self.demod_registers = Registers(
            'demod',
            {
                0b00: Register('demod_status', [
                    Field('dibit_count', Access.R, 16, 0),
                    Field('demod_overflow', Access.Rsticky, 1, 0),
                    Field('last_buffer', Access.R,
                          config.dibit_dma_num_buffers_log2, -1),
                ]),
                0b01: Register('demod_control', [
                    # Single enable bit for ring DMA (replaces start/stop pulses)
                    Field('demod_enable', Access.RW, 1, 0),
                ]),
                0b10: Register('dibit_next_address', [
                    Field('next_address', Access.R, 32, 0),
                ]),
            },
            2)

        # ── Control-channel post-DDC IQ ring DMA (Phase 6C) ───────────
        # Third tap of the control DDC output (alongside c4fm_demod and
        # symbol_timing). The packer buffers two consecutive (re, im)
        # pairs into a 64-bit DMA word; see iq_packer.py for the bit
        # layout. The DMA mirrors the dibit_dma pattern: continuous
        # ring write, sub-buffer-completion interrupt, level-enable.
        #
        # Address: 0x1900_0000 / 256 KB ring (8 x 32 KB sub-buffers).
        # See doc/P25_ADDRESS_MAP.md for the full picture.
        self.iq_packer = IQPacker()
        self.iq_dma = DmaStreamRingWrite(
            config.iq_dma_address,
            config.iq_dma_num_buffers_log2,
            config.iq_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_iq')

        # ── Control channel IQ DMA registers (0x80) ───────────────────
        # Bank 4 in the AXI-Lite register space (next free bank after
        # control/sdr/demod/traffic). The bank decoder in elaborate()
        # already covers bits [5:3] of the word address (3 bits = 8
        # banks max), so this slot does not require any address-width
        # change. See doc/P25_ADDRESS_MAP.md for the bank table.
        #
        # Layout intentionally mirrors the demod_status / dibit_next_address
        # pair from the control-channel block, with iq_overflow at bit 0
        # (Rsticky, clears on read) and last_buffer at bits [16+:N] to
        # leave the low half free for future flags.
        self.iq_registers = Registers(
            'iq', {
                0b00: Register('iq_dma_status', [
                    Field('iq_overflow', Access.Rsticky, 1, 0),
                    Field('last_buffer', Access.R,
                          config.iq_dma_num_buffers_log2, -1),
                ]),
                0b01: Register('iq_dma_control', [
                    # Single enable bit for ring DMA AW channel
                    # (level signal, not pulse) - mirrors dibit_dma.
                    Field('iq_enable', Access.RW, 1, 0),
                ]),
                0b10: Register('iq_next_address', [
                    Field('next_address', Access.R, 32, 0),
                ]),
            },
            2)

        # ── Control channel LSM demod chain (Phase 6E.9) ──────────────
        # Sits in parallel with the C4FM chain on the control channel.
        # Both consume the same control DDC output (62.5 kSPS, 16-bit
        # signed I+Q). The LSM chain decimates by 2 down to 31.25 kSPS,
        # filters with the 83-tap baseband LPF and the 105-tap RRC,
        # then runs LsmDemod (timing recovery + diff demod + Costas
        # PLL rotate + slicer + sync detect + BCH FEC). The recovered
        # dibits exit via lsm_dibit_dma; the recovered NIDs are
        # surfaced via the new `lsm` register bank.
        #
        # All four blocks live at top level (not wrapped) for the same
        # reason the C4FM chain does: keeps each block visible in the
        # Vivado hierarchy and amaranth-sim waveforms during bring-up.
        #
        # Resource budget per the 6E.8 estimate (doc 017):
        #   LsmDecimator2  : ~30 LUT, 0 DSP, 0 BRAM
        #   LsmFir x 2     : ~2 DSP48 (sequential MAC, one per FIR),
        #                    0 BRAM (taps live in distributed ROM)
        #   LsmDemod       : ~30 DSP48, 2 BRAM18, ~3940 LUT
        #   Total          : ~32 DSP48 (15% of Z7020), 2 BRAM18 (1.4%)
        self.lsm_decimator = LsmDecimator2(width=16)
        self.lsm_lpf = LsmFir(LPF_TAPS_31250)
        self.lsm_rrc = LsmFir(RRC_TAPS_31250)
        self.lsm_demod = LsmDemod()
        # Reuse the existing DibitPacker for the LSM dibit stream so
        # the LSM ring DMA word format is bit-identical to the C4FM
        # ring DMA -- the PS reads both rings the same way.
        self.lsm_dibit_packer = DibitPacker()
        self.lsm_dibit_dma = DmaStreamRingWrite(
            config.lsm_dibit_dma_address,
            config.lsm_dibit_dma_num_buffers_log2,
            config.lsm_dibit_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_lsm_dibit')

        # ── LSM register bank (0xA0, bank 5) ──────────────────────────
        # See doc/P25_ADDRESS_MAP.md for the canonical layout, including
        # the read/write semantics of every field and the PS-side polling
        # protocol for NID events.
        #
        # Bit-layout shorthand (bit positions inside each 32-bit word):
        #   lsm_control [0]   lsm_enable
        #               [1]   lsm_dibit_dma_enable
        #               [2]   lsm_dc_block_enable   (Phase 6G.1)
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
                    # LsmDemod chain (PLL accumulator + timing +
                    # diff slicer + sync register + BCH sweep state
                    # all clear back to init). Self-clearing on the
                    # next sync cycle -- reads as 0. Wired below in
                    # the LSM chain block. See
                    # doc/changes/038_phase8_runtime_reset.md.
                    Field('lsm_reset', Access.Wpulse, 1, 0),
                    # Phase 6G.1: front-end DC blocker enable.
                    # Defaults to 0 (off) at reset to match the
                    # convention of lsm_enable; PS-side code is
                    # responsible for setting this to 1 in the
                    # same write that turns on lsm_enable.
                    Field('lsm_dc_block_enable', Access.RW, 1, 0),
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
            },
            3)

        # ── Traffic channel DDC + demod chain ─────────────────────────
        self.traffic_ddc = DDC('clk3x')
        self.traffic_c4fm = C4FMDemod()
        self.traffic_timing = SymbolTimingRecovery(samples_per_symbol=13)
        self.traffic_packer = DibitPacker()
        self.traffic_dma = DmaStreamRingWrite(
            config.traffic_dma_address,
            config.traffic_dma_num_buffers_log2,
            config.traffic_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_traffic')

        # ── Traffic channel registers (0x60) ──────────────────────────
        # Independent DDC NCO + decimation for PS-controlled retuning
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
                0b011: Register(
                    'traffic_demod_status', [
                        Field('dibit_count', Access.R, 16, 0),
                        Field('demod_overflow', Access.Rsticky, 1, 0),
                        Field('last_buffer', Access.R,
                              config.traffic_dma_num_buffers_log2, -1),
                    ]),
                0b100: Register(
                    'traffic_demod_control', [
                        Field('demod_enable', Access.RW, 1, 0),
                    ]),
                0b101: Register(
                    'traffic_next_address', [
                        Field('next_address', Access.R, 32, 0),
                    ]),
            }, 3)

        # ── Traffic channel LSM demod chain (Phase 7A.2) ──────────────
        # Sits in parallel with the C4FM traffic chain on the traffic
        # side, identical structure to the control-side LSM chain
        # (Phase 6E.9). Both consume the same `traffic_ddc` output;
        # the LSM chain decimates by 2 (62.5 -> 31.25 kSPS), runs the
        # 83-tap baseband LPF and the 105-tap RRC matched filter,
        # then feeds LsmDemod which does timing recovery + diff demod
        # + Costas-style PLL rotate + slicer + sync detect + BCH FEC.
        # Recovered dibits exit via `traffic_lsm_dibit_dma`; recovered
        # NIDs are surfaced via the new `traffic_lsm` register bank
        # (bank 6 at offset 0xC0).
        #
        # Phase 7A.2 is the FPGA prerequisite for HDU + TDU detection
        # on the voice channel. Once the LSM chain is producing NID
        # events on the traffic side, the PS dispatcher classifies
        # each DUID:
        #
        #   0x0  HDU         -> call start
        #   0x3  TDU         -> call end
        #   0x5  LDU1        -> voice + Link Control     -> activity
        #   0xA  LDU2        -> voice + Encryption Sync  -> activity
        #   0xF  TDU_LC      -> call end with LC payload
        #
        # IMBE frame extraction (Phase 7C) and the vocoder (Phase 7D)
        # build on this same NID + dibit infrastructure.
        #
        # Resource budget per the 6E.8 estimate (doc 017), same numbers
        # as the control-side LSM chain because the blocks are
        # bit-identical:
        #   LsmDecimator2  : ~30 LUT, 0 DSP, 0 BRAM
        #   LsmFir x 2     : ~2 DSP48 (sequential MAC, one per FIR)
        #   LsmDemod       : ~30 DSP48, 2 BRAM18, ~3940 LUT
        #   Total          : ~32 DSP48, 2 BRAM18 (~14% / ~1.4% of Z7020)
        #
        # Z7020 has plenty of room: control-side LSM was ~25% LUT
        # and ~16% DSP at Phase 6G.1, so the doubled total still
        # leaves >50% headroom for the Phase 7G channelizer.
        self.traffic_lsm_decimator = LsmDecimator2(width=16)
        self.traffic_lsm_lpf = LsmFir(LPF_TAPS_31250)
        self.traffic_lsm_rrc = LsmFir(RRC_TAPS_31250)
        self.traffic_lsm_demod = LsmDemod()
        # Reuse DibitPacker so the new ring DMA word format is
        # bit-identical to the control LSM ring DMA -- the PS reads
        # both rings the same way.
        self.traffic_lsm_dibit_packer = DibitPacker()
        self.traffic_lsm_dibit_dma = DmaStreamRingWrite(
            config.traffic_lsm_dibit_dma_address,
            config.traffic_lsm_dibit_dma_num_buffers_log2,
            config.traffic_lsm_dibit_dma_buffer_size,
            width=64, axi_awidth=32, name='m_axi_traffic_lsm_dibit')

        # ── Traffic LSM register bank (0xC0, bank 6) ──────────────────
        # Identical layout to the control-side `lsm` bank (0xA0).
        # Field semantics are also identical -- the PS-side dispatcher
        # for HDU/TDU/LDU events polls this bank with the same
        # heartbeat pattern as the control-side `lsm_status` poll.
        #
        # Bit-layout shorthand (bit positions inside each 32-bit word):
        #   traffic_lsm_control [0]   traffic_lsm_enable
        #                       [1]   traffic_lsm_dibit_dma_enable
        #                       [2]   traffic_lsm_dc_block_enable
        #   traffic_lsm_status  [0]   bch_busy
        #                       [1]   in_nid_window
        #                       [2]   nid_event             (Rsticky)
        #                       [3]   nid_valid
        #                       [10:4]  n_errors            (7 bits)
        #                       [17:11] sync_distance       (7 bits)
        #                       [18]  traffic_lsm_dibit_overflow (Rsticky)
        #   traffic_lsm_nid     [11:0]  nac                 (12 bits)
        #                       [15:12] duid                (4 bits)
        #   traffic_lsm_drop_count
        #                       [15:0]  drop_count          (16 bits)
        #                       [18:16] traffic_lsm_dibit_last_buffer
        #                                                   (3 bits, init -1)
        #   traffic_lsm_dibit_next [31:0] next_address       (32 bits)
        #   traffic_lsm_debug   [15:0]  pll_dbg             (signed Q2.13)
        #                       [31:16] sample_point_dbg    (signed Q4.10)
        self.traffic_lsm_registers = Registers(
            'traffic_lsm', {
                0b000: Register('traffic_lsm_control', [
                    Field('traffic_lsm_enable', Access.RW, 1, 0),
                    Field('traffic_lsm_dibit_dma_enable', Access.RW, 1, 0),
                    Field('traffic_lsm_dc_block_enable', Access.RW, 1, 0),
                    # Phase 8A: W1P runtime-reset strobe for the
                    # traffic LSM chain. Writing 1 pulses the
                    # LsmDemod reset_in port and clears the PLL
                    # accumulator, timing state, diff slicer prev,
                    # sync register, and BCH sweep state. The
                    # Phase 8B retune path toggles this on every
                    # traffic-DDC retune so the post-retune
                    # acquisition starts from a clean cold-start.
                    # See doc/changes/038_phase8_runtime_reset.md.
                    Field('traffic_lsm_reset', Access.Wpulse, 1, 0),
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
            },
            3)

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
        # Address banks: bits [5:3] of the word address select the bank
        # (3 bits = 8 banks max). Each bank spans 8 words = 32 bytes (0x20).
        # See doc/P25_ADDRESS_MAP.md for the canonical bank table.
        self.register_map = RegisterMap({
            0x00: self.control_registers,
            0x20: self.sdr_registers,
            0x40: self.demod_registers,
            0x60: self.traffic_registers,
            0x80: self.iq_registers,        # Phase 6C
            0xA0: self.lsm_registers,       # Phase 6E.9
            0xC0: self.traffic_lsm_registers,  # Phase 7A.2
        }, metadata)

        # ── I/O signals ────────────────────────────────────────────────
        self.iq_in_width = 12
        self.re_in = Signal(self.iq_in_width)
        self.im_in = Signal(self.iq_in_width)
        self.interrupt_out = Signal()

    def ports(self):
        return (
            self.axi4lite.axi.ports()
            + self.dibit_dma.axi.ports()
            + self.traffic_dma.axi.ports()
            + self.traffic_lsm_dibit_dma.axi.ports()  # Phase 7A.2
            + self.iq_dma.axi.ports()       # Phase 6C
            + self.lsm_dibit_dma.axi.ports()  # Phase 6E.9
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
        ]

        # Phase 8C: local clock domains for the two LSM demod
        # chains. Both use the `sync` clock (same edges), but each
        # has its own synchronous reset wired to `~lsm_enable` of
        # the respective chain. The net effect: toggling
        # `lsm_control.lsm_enable` or
        # `traffic_lsm_control.traffic_lsm_enable` becomes a full
        # reset of every register in that chain that has a reset
        # wire -- FSM state in LsmSyncNidExtract / LsmNidBchFec /
        # CORDIC, stage strobes in LsmPllUpdate, output latches,
        # and the dibit/symbol strobe registers. Registers declared
        # with `reset_less=True` (pll_reg in LsmPllUpdate,
        # sample_point's FIFO entries in LsmTimingInterp,
        # diff-slicer prev_*, sync_reg, etc.) do NOT clear on
        # domain reset -- the explicit `reset_in` path added in
        # Phase 8A is still load-bearing for those. The two paths
        # are complementary: domain reset handles pipeline state
        # automatically, explicit reset_in handles the reset_less
        # persistent state.
        #
        # Clock drive + reset wire for these domains happens below,
        # once `lsm_enable` / `traffic_lsm_enable` are in scope.
        # NOTE: Amaranth deprecated the `local=True` flag -- all
        # named domains are local to their module by default, so
        # just construct unnamed `ClockDomain`s here.
        #
        # Phase 8C.1 (2026-04-15) regression revert: the control-side
        # `lsm_ctrl_dom` wrap was bypassed for the next bake. On-target
        # measurement showed the control-side LSM chain regressed from
        # the Phase 6F.9 baseline of 91.7% TSBK CRC pass to 24.8% pass
        # after the Phase 8C wrap landed -- with a 60/40 inner/outer
        # dibit ratio and per-NID `pll_dbg` swinging by thousands of
        # ULPs (PLL hunting in steady state). The traffic side keeps
        # its `lsm_traffic_dom` wrap because Phase 8B's per-call
        # retune flow needs `traffic_lsm_enable` toggling to clear
        # non-`reset_less` state between calls. If this revert
        # restores the control-side CRC pass rate to the 6F.9
        # baseline, Phase 8C is the confirmed cause and we'll need
        # a different mechanism than `DomainRenamer` for the Phase
        # 7G channel-hop infrastructure 8C was prepping for.
        lsm_traffic_dom = ClockDomain("lsm_traffic_dom")
        m.domains += [lsm_traffic_dom]
        lsm_traffic_renamer = DomainRenamer({'sync': 'lsm_traffic_dom'})

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

        # ── C4FM Demod chain ──────────────────────────────────────────
        m.submodules.c4fm_demod = self.c4fm_demod
        m.submodules.symbol_timing = self.symbol_timing
        m.submodules.dibit_packer = self.dibit_packer
        m.submodules.dibit_dma = self.dibit_dma
        m.submodules.demod_registers = self.demod_registers
        m.submodules.demod_registers_cdc = demod_registers_cdc = RegisterCDC(
            's_axi_lite', 'sync', self.demod_registers.aw)

        # Phase 6C: control-channel post-DDC IQ ring DMA submodules.
        # Both run in the same sync domain as dibit_dma and tap the
        # same DDC output below.
        m.submodules.iq_packer = self.iq_packer
        m.submodules.iq_dma = self.iq_dma
        m.submodules.iq_registers = self.iq_registers
        m.submodules.iq_registers_cdc = iq_registers_cdc = RegisterCDC(
            's_axi_lite', 'sync', self.iq_registers.aw)

        # Phase 6E.9: control-channel LSM demod chain submodules.
        # All run in the same `sync` domain alongside the C4FM
        # chain. Phase 8C tried to wrap `lsm_demod` in its own
        # `lsm_ctrl_dom` local clock domain (so disabling
        # `lsm_enable` would force a synchronous reset of the
        # non-`reset_less` pipeline state), but on-target testing
        # showed a 67-percentage-point TSBK CRC pass-rate
        # regression after that wrap landed -- 91.7% (Phase 6F.9
        # baseline) → 24.8%. Phase 8C.1 reverts the wrap on the
        # control side; the traffic side keeps `lsm_traffic_dom`
        # because Phase 8B's per-call retune flow needs it. See
        # the comment on `lsm_traffic_dom` near the top of
        # `elaborate()` for the full rationale.
        m.submodules.lsm_decimator = self.lsm_decimator
        m.submodules.lsm_lpf = self.lsm_lpf
        m.submodules.lsm_rrc = self.lsm_rrc
        m.submodules.lsm_demod = self.lsm_demod
        m.submodules.lsm_dibit_packer = self.lsm_dibit_packer
        m.submodules.lsm_dibit_dma = self.lsm_dibit_dma
        m.submodules.lsm_registers = self.lsm_registers
        m.submodules.lsm_registers_cdc = lsm_registers_cdc = RegisterCDC(
            's_axi_lite', 'sync', self.lsm_registers.aw)

        # DDC output -> C4FM discriminator
        m.d.comb += [
            self.c4fm_demod.re_in.eq(self.ddc.re_out),
            self.c4fm_demod.im_in.eq(self.ddc.im_out),
            self.c4fm_demod.strobe_in.eq(self.ddc.strobe_out),
        ]

        # Symbol timing + slicer
        # - Raw post-DDC IQ samples feed the symbol-rate differential
        #   slicer inside SymbolTimingRecovery (4 DSP48E1 multiplies)
        # - diff_im (the FM cross-product from C4FMDemod) feeds the
        #   Gardner TED for clock recovery
        # The slicer computes z_sym[k] * conj(z_sym[k-1]) at the
        # symbol decision point; sign bits give the 4-quadrant dibit.
        m.d.comb += [
            self.symbol_timing.re_in.eq(self.ddc.re_out),
            self.symbol_timing.im_in.eq(self.ddc.im_out),
            self.symbol_timing.diff_im_in.eq(self.c4fm_demod.diff_im_out),
            self.symbol_timing.strobe_in.eq(self.c4fm_demod.strobe_out),
        ]

        # Symbol timing -> Dibit packer
        m.d.comb += [
            self.dibit_packer.dibit_in.eq(self.symbol_timing.dibit_out),
            self.dibit_packer.symbol_strobe.eq(
                self.symbol_timing.symbol_strobe),
        ]

        # Dibit packer -> ring DMA stream
        m.d.comb += [
            self.dibit_dma.stream_data.eq(self.dibit_packer.data_out),
            self.dibit_dma.stream_valid.eq(self.dibit_packer.data_valid),
            self.dibit_packer.stream_ready.eq(self.dibit_dma.stream_ready),
        ]

        # Ring DMA enable from demod_control register (level, not pulse)
        m.d.comb += [
            self.dibit_dma.enable.eq(
                self.demod_registers['demod_control']['demod_enable']),
        ]

        # DMA sub-buffer completion -> interrupt (sticky bit, cleared by read)
        interrupts_reg = self.control_registers['interrupts']
        m.d.comb += [
            interrupts_reg['dibit_dma'].eq(self.dibit_dma.interrupt),
        ]

        # Demod status registers
        dibit_counter = Signal(16, reset_less=True)
        with m.If(self.symbol_timing.symbol_strobe):
            m.d.sync += dibit_counter.eq(dibit_counter + 1)
        m.d.comb += [
            self.demod_registers['demod_status']['dibit_count'].eq(
                dibit_counter),
            self.demod_registers['demod_status']['demod_overflow'].eq(
                self.dibit_packer.overflow),
            self.demod_registers['demod_status']['last_buffer'].eq(
                self.dibit_dma.last_buffer),
            # next_address is no longer exposed by ring DMA — report
            # the AW write address from inside the AXI interface for debug.
            self.demod_registers['dibit_next_address']['next_address'].eq(
                self.dibit_dma.axi.awaddr),
        ]

        # ── Control-channel IQ ring DMA (Phase 6C) ────────────────────
        # Third tap of the control DDC output. The packer fans the
        # same re_out / im_out / strobe_out signals that already feed
        # c4fm_demod and symbol_timing into a 64-bit DMA stream
        # (two IQ pairs per word, sample 0 in the low half).
        # See iq_packer.py for the bit layout and
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
            interrupts_reg['iq_dma'].eq(self.iq_dma.interrupt),
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
        #                   nid event registers }
        #
        # Master enable: lsm_control.lsm_enable gates the strobe at the
        # very front of LsmDecimator2, so when 0 every downstream block
        # sees no strobes and goes quiescent (no PLL drift, no BCH
        # sweeps, no spurious dibits).
        lsm_enable = self.lsm_registers['lsm_control']['lsm_enable']

        # Phase 8C.1 (2026-04-15) revert: the lsm_ctrl_dom wiring
        # used to live here. With the control-side LsmDemod back in
        # the global `sync` domain, there's no separate clock or
        # reset to drive. The `lsm_demod.reset_in` Phase 8A wiring
        # below still works because the Phase 8A override block in
        # LsmPllUpdate / LsmTimingInterp / etc. is purely
        # `m.d.sync` (now bound to the global sync domain instead
        # of the per-chain lsm_ctrl_dom).

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
        # `dibit_out`/`symbol_strobe` (passthrough to dibit DMA) and
        # `nid_event_strobe` + (NAC, DUID, ...) latched into the
        # lsm register bank below.
        #
        # Phase 6G.1: dc_block_enable is driven from the new
        # lsm_control.lsm_dc_block_enable PS bit. Defaults to 0 at
        # reset; the P25 daemon's `lsm_control` write turns it on
        # at the same time as `lsm_enable`.
        m.d.comb += [
            self.lsm_demod.re_in.eq(self.lsm_rrc.re_out),
            self.lsm_demod.im_in.eq(self.lsm_rrc.im_out),
            self.lsm_demod.strobe_in.eq(self.lsm_rrc.strobe_out),
            self.lsm_demod.dc_block_enable.eq(
                self.lsm_registers['lsm_control']['lsm_dc_block_enable']),
            # Phase 8A: runtime reset strobe. `lsm_reset` is a
            # W1P field, so the Register machinery gives us a
            # clean 1-sync-cycle pulse per PS write -- feed it
            # directly into LsmDemod.reset_in.
            self.lsm_demod.reset_in.eq(
                self.lsm_registers['lsm_control']['lsm_reset']),
        ]

        # Stage 5: LsmDemod dibits -> packer -> ring DMA stream
        # Mirrors the C4FM dibit_packer / dibit_dma wiring exactly.
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
            interrupts_reg['lsm_dibit_dma'].eq(self.lsm_dibit_dma.interrupt),
        ]

        # Stage 6: NID event latching.
        # Latched copies of the BCH-decoded NID fields. Updated on
        # every nid_event_strobe pulse so the PS sees a coherent
        # snapshot when it reads after observing nid_event sticky.
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

        # Continuous-read live signals + the latched NID fields, all
        # surfaced as R / Rsticky fields in the lsm bank. The
        # nid_event sticky bit is fed by nid_event_strobe directly --
        # the Rsticky machinery in `Register` does the latch + clear-
        # on-read.
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
            # pll_dbg is signed Q2.13 (16 bits), drop straight in.
            lsm_debug['pll_dbg'].eq(self.lsm_demod.pll_dbg),
            # sample_point_dbg is signed Q4.12 (18 bits); take the top
            # 16 bits to get a Q4.10 view that fits the field. Losing
            # 2 LSBs of fractional resolution is fine for a dashboard
            # trace -- still ~0.25 sample of precision.
            lsm_debug['sample_point_dbg'].eq(
                self.lsm_demod.sample_point_dbg[2:]),
        ]

        # ── Traffic channel DDC + demod chain ─────────────────────────
        m.submodules.traffic_ddc = self.traffic_ddc
        m.submodules.traffic_c4fm = self.traffic_c4fm
        m.submodules.traffic_timing = self.traffic_timing
        m.submodules.traffic_packer = self.traffic_packer
        m.submodules.traffic_dma = self.traffic_dma
        m.submodules.traffic_registers = self.traffic_registers
        m.submodules.traffic_registers_cdc = traffic_registers_cdc = RegisterCDC(
            's_axi_lite', 'sync', self.traffic_registers.aw)

        # Traffic DDC shares the same IQ input as control DDC
        # but has its own NCO frequency for independent tuning
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

        # Traffic DDC -> differential demod -> timing -> packer -> ring DMA
        # Same C4FM/LSM-unified differential approach as the control chain:
        # raw IQ feeds the symbol-rate slicer, diff_im feeds Gardner TED.
        m.d.comb += [
            self.traffic_c4fm.re_in.eq(self.traffic_ddc.re_out),
            self.traffic_c4fm.im_in.eq(self.traffic_ddc.im_out),
            self.traffic_c4fm.strobe_in.eq(self.traffic_ddc.strobe_out),
            self.traffic_timing.re_in.eq(self.traffic_ddc.re_out),
            self.traffic_timing.im_in.eq(self.traffic_ddc.im_out),
            self.traffic_timing.diff_im_in.eq(self.traffic_c4fm.diff_im_out),
            self.traffic_timing.strobe_in.eq(self.traffic_c4fm.strobe_out),
            self.traffic_packer.dibit_in.eq(self.traffic_timing.dibit_out),
            self.traffic_packer.symbol_strobe.eq(
                self.traffic_timing.symbol_strobe),
            self.traffic_dma.stream_data.eq(self.traffic_packer.data_out),
            self.traffic_dma.stream_valid.eq(self.traffic_packer.data_valid),
            self.traffic_packer.stream_ready.eq(
                self.traffic_dma.stream_ready),
            self.traffic_dma.enable.eq(
                self.traffic_registers['traffic_demod_control']['demod_enable']),
        ]

        # Traffic DMA sub-buffer completion -> interrupt
        m.d.comb += [
            interrupts_reg['traffic_dma'].eq(self.traffic_dma.interrupt),
        ]

        # Traffic demod status registers
        traffic_dibit_counter = Signal(16, reset_less=True)
        with m.If(self.traffic_timing.symbol_strobe):
            m.d.sync += traffic_dibit_counter.eq(
                traffic_dibit_counter + 1)
        m.d.comb += [
            self.traffic_registers['traffic_demod_status']['dibit_count'].eq(
                traffic_dibit_counter),
            self.traffic_registers['traffic_demod_status']['demod_overflow'].eq(
                self.traffic_packer.overflow),
            self.traffic_registers['traffic_demod_status']['last_buffer'].eq(
                self.traffic_dma.last_buffer),
            self.traffic_registers['traffic_next_address']['next_address'].eq(
                self.traffic_dma.axi.awaddr),
        ]

        # ── Traffic-channel LSM demod chain (Phase 7A.2) ──────────────
        # Pipeline (mirror of the control-side LSM chain at Phase 6E.9):
        #   traffic_ddc (62.5 kSPS) -> /2 decimator -> LPF -> RRC ->
        #     LsmDemod -> { dibit_packer -> traffic_lsm_dibit_dma,
        #                   nid event registers (traffic_lsm bank) }
        #
        # Master enable: traffic_lsm_control.traffic_lsm_enable gates
        # the strobe at the very front of LsmDecimator2, so when 0
        # every downstream block sees no strobes and goes quiescent.
        # Identical semantics to the control-side lsm_enable.
        #
        # Phase 7A.2 is the FPGA prerequisite for HDU + TDU detection
        # on the voice channel. The PS-side dispatcher reads
        # traffic_lsm_status (and optionally drains traffic_lsm_dibit_dma
        # for IMBE extraction in Phase 7C) to classify each NID:
        #
        #   DUID 0x0  HDU         -> call start
        #   DUID 0x3  TDU         -> call end (release lock)
        #   DUID 0x5  LDU1        -> voice + LC, refresh activity
        #   DUID 0xA  LDU2        -> voice + ESS, refresh activity
        #   DUID 0xF  TDU_LC      -> call end with LC payload
        # Phase 8C: traffic-side LsmDemod moved into its own
        # `lsm_traffic_dom` local clock domain (see the control
        # side above for the rationale). The decimator / LPF /
        # RRC / packer / DMA stay in `sync` so the upstream DDC
        # strobe + downstream DMA master state survive across
        # per-call LSM chain resets.
        m.submodules.traffic_lsm_decimator = self.traffic_lsm_decimator
        m.submodules.traffic_lsm_lpf = self.traffic_lsm_lpf
        m.submodules.traffic_lsm_rrc = self.traffic_lsm_rrc
        m.submodules.traffic_lsm_demod = lsm_traffic_renamer(
            self.traffic_lsm_demod)
        m.submodules.traffic_lsm_dibit_packer = self.traffic_lsm_dibit_packer
        m.submodules.traffic_lsm_dibit_dma = self.traffic_lsm_dibit_dma
        m.submodules.traffic_lsm_registers = self.traffic_lsm_registers
        m.submodules.traffic_lsm_registers_cdc = traffic_lsm_registers_cdc = \
            RegisterCDC(
                's_axi_lite', 'sync', self.traffic_lsm_registers.aw)

        traffic_lsm_enable = self.traffic_lsm_registers[
            'traffic_lsm_control']['traffic_lsm_enable']

        # Phase 8C: wire the traffic LSM local clock domain's
        # clock + reset. Mirrors the control side exactly -- same
        # `sync` clock, reset driven by `~traffic_lsm_enable`.
        m.d.comb += [
            ClockSignal("lsm_traffic_dom").eq(ClockSignal("sync")),
            ResetSignal("lsm_traffic_dom").eq(~traffic_lsm_enable),
        ]

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
        # Stage 4: RRC -> LsmDemod (timing recovery + diff demod +
        # Costas-style PLL rotate + slicer + sync detect + BCH FEC).
        # Identical block to the control-side LsmDemod -- bit-identical
        # behavior, just fed by the traffic DDC.
        m.d.comb += [
            self.traffic_lsm_demod.re_in.eq(self.traffic_lsm_rrc.re_out),
            self.traffic_lsm_demod.im_in.eq(self.traffic_lsm_rrc.im_out),
            self.traffic_lsm_demod.strobe_in.eq(
                self.traffic_lsm_rrc.strobe_out),
            self.traffic_lsm_demod.dc_block_enable.eq(
                self.traffic_lsm_registers[
                    'traffic_lsm_control']['traffic_lsm_dc_block_enable']),
            # Phase 8A: runtime reset strobe for the traffic
            # chain. `traffic_lsm_reset` is W1P, so a PS write of
            # 1 gives us a clean 1-sync-cycle pulse that clears
            # the PLL accumulator, timing state, diff slicer
            # prev history, sync register, and any in-flight BCH
            # sweep. The Phase 8B PS retune path toggles this on
            # every traffic-DDC retune.
            self.traffic_lsm_demod.reset_in.eq(
                self.traffic_lsm_registers[
                    'traffic_lsm_control']['traffic_lsm_reset']),
        ]

        # Stage 5: LsmDemod dibits -> packer -> ring DMA stream.
        # Mirrors the control-side LSM dibit_packer / dibit_dma wiring
        # exactly. Phase 7C will tap traffic_lsm_demod.dibit_out /
        # symbol_strobe in PARALLEL to feed an LDU/IMBE extractor; the
        # ring DMA path is independent of that future tap.
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
                self.traffic_lsm_dibit_dma.interrupt),
        ]

        # Stage 6: NID event latching (mirror of control side).
        # Latched copies of the BCH-decoded NID fields. Updated on
        # every nid_event_strobe pulse so the PS sees a coherent
        # snapshot when it reads after observing nid_event sticky.
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

        # ── Register crossbar ─────────────────────────────────────────
        # Address map (word-addressed via AXI4-Lite, 7-bit address):
        # Bank field is bits [5:3] of the word address (3 bits = 8
        # banks max). See doc/P25_ADDRESS_MAP.md for the canonical table.
        #
        #   word 0x00-0x07: control registers      (bits [5:3] == 000)
        #   word 0x08-0x0F: SDR/DDC registers      (bits [5:3] == 001)
        #   word 0x10-0x17: demod registers        (bits [5:3] == 010)
        #   word 0x18-0x1F: traffic registers      (bits [5:3] == 011)
        #   word 0x20-0x27: IQ DMA registers       (bits [5:3] == 100) [Phase 6C]
        #   word 0x28-0x2F: LSM registers          (bits [5:3] == 101) [Phase 6E.9]
        #   word 0x30-0x37: traffic_lsm registers  (bits [5:3] == 110) [Phase 7A.2]
        #   word 0x38-0x3F: free for future banks
        address = Signal(self.axi4_awidth, reset_less=True)
        wdata = Signal(32, reset_less=True)
        addr_bank = self.axi4lite.address[3:6]  # bits [5:3]
        control_regs_select = (addr_bank == 0b000)
        sdr_regs_select = (addr_bank == 0b001)
        demod_regs_select = (addr_bank == 0b010)
        traffic_regs_select = (addr_bank == 0b011)
        iq_regs_select = (addr_bank == 0b100)       # Phase 6C
        lsm_regs_select = (addr_bank == 0b101)      # Phase 6E.9
        traffic_lsm_regs_select = (addr_bank == 0b110)   # Phase 7A.2
        m.d.s_axi_lite += [
            self.axi4lite.rdata.eq(self.control_registers.rdata
                                   | sdr_registers_cdc.i_rdata
                                   | demod_registers_cdc.i_rdata
                                   | traffic_registers_cdc.i_rdata
                                   | iq_registers_cdc.i_rdata
                                   | lsm_registers_cdc.i_rdata
                                   | traffic_lsm_registers_cdc.i_rdata),
            self.axi4lite.rdone.eq(self.control_registers.rdone
                                   | sdr_registers_cdc.i_rdone
                                   | demod_registers_cdc.i_rdone
                                   | traffic_registers_cdc.i_rdone
                                   | iq_registers_cdc.i_rdone
                                   | lsm_registers_cdc.i_rdone
                                   | traffic_lsm_registers_cdc.i_rdone),
            self.axi4lite.wdone.eq(self.control_registers.wdone
                                   | sdr_registers_cdc.i_wdone
                                   | demod_registers_cdc.i_wdone
                                   | traffic_registers_cdc.i_wdone
                                   | iq_registers_cdc.i_wdone
                                   | lsm_registers_cdc.i_wdone
                                   | traffic_lsm_registers_cdc.i_wdone),
            self.control_registers.ren.eq(
                self.axi4lite.ren & control_regs_select),
            self.control_registers.wstrobe.eq(
                Mux(control_regs_select, self.axi4lite.wstrobe, 0)),
            sdr_registers_cdc.i_ren.eq(
                self.axi4lite.ren & sdr_regs_select),
            sdr_registers_cdc.i_wstrobe.eq(
                Mux(sdr_regs_select, self.axi4lite.wstrobe, 0)),
            demod_registers_cdc.i_ren.eq(
                self.axi4lite.ren & demod_regs_select),
            demod_registers_cdc.i_wstrobe.eq(
                Mux(demod_regs_select, self.axi4lite.wstrobe, 0)),
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
            address.eq(self.axi4lite.address),
            wdata.eq(self.axi4lite.wdata),
        ]
        m.d.comb += [
            self.control_registers.address.eq(address),
            self.control_registers.wdata.eq(wdata),
            sdr_registers_cdc.i_address.eq(address),
            sdr_registers_cdc.i_wdata.eq(wdata),
            demod_registers_cdc.i_address.eq(address),
            demod_registers_cdc.i_wdata.eq(wdata),
            traffic_registers_cdc.i_address.eq(address),
            traffic_registers_cdc.i_wdata.eq(wdata),
            iq_registers_cdc.i_address.eq(address),
            iq_registers_cdc.i_wdata.eq(wdata),
            lsm_registers_cdc.i_address.eq(address),
            lsm_registers_cdc.i_wdata.eq(wdata),
            traffic_lsm_registers_cdc.i_address.eq(address),
            traffic_lsm_registers_cdc.i_wdata.eq(wdata),
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
        # demod_registers CDC
        m.d.comb += [
            self.demod_registers.ren.eq(demod_registers_cdc.o_ren),
            self.demod_registers.wstrobe.eq(demod_registers_cdc.o_wstrobe),
            self.demod_registers.address.eq(demod_registers_cdc.o_address),
            self.demod_registers.wdata.eq(demod_registers_cdc.o_wdata),
            demod_registers_cdc.o_rdone.eq(self.demod_registers.rdone),
            demod_registers_cdc.o_wdone.eq(self.demod_registers.wdone),
            demod_registers_cdc.o_rdata.eq(self.demod_registers.rdata),
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

        # ── Internal resets ───────────────────────────────────────────
        for internal in ['sync', 'clk3x', 'sampling']:
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
