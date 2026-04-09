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
# CURRENT STATUS: this gateware decodes C4FM only. The LSM path is NOT
# YET IMPLEMENTED -- if you point this at a simulcast system you will
# get random NIDs and no useful decode. See doc/changes/0xx_lsm_support.md
# (TBD) for the design discussion.
#
# History of this comment block (for context, since it has been wrong
# in informative ways before):
#
# An earlier iteration of this design claimed the per-sample differential
# z[n]*conj(z[n-1]) (specifically the sign bits of (diff_re, diff_im))
# was a "unified" slicer that worked for both C4FM and LSM. This was
# wrong in two important ways:
#
#   1. The per-sample differential at ~13 samples/symbol has cos(small)
#      ~+1 always, so diff_re never goes negative. Only 2 of the 4 dibit
#      values appear. Fixed by computing the differential at SYMBOL rate
#      (sym[k]*conj(sym[k-1])) where the phase change is the actual P25
#      symbol angle of +-pi/4 or +-3pi/4. See SymbolTimingRecovery's
#      symbol-rate slicer block for the working version.
#
#   2. Even with the symbol-rate fix, the slicer only decodes C4FM
#      cleanly. For LSM it produces a near-random dibit stream because:
#        (a) LSM pulses are shaped (raised-cosine) so adjacent symbols
#            ISI into each decision -- needs an RRC matched filter
#            *before* slicing. We don't have one.
#        (b) LSM data lives in absolute carrier phase, so any LO offset
#            between AD9361 and the transmitter rotates the constellation
#            continuously. The slicer's fixed reference angle drifts and
#            sym_diff_re/sym_diff_im sweep through all 4 quadrants
#            independent of signal content. Needs a Costas loop (or
#            equivalent coherent carrier recovery). We don't have one.
#
#      Empirically confirmed 2026-04-09 against the Clay County simulcast
#      site (NAC 0x8A1, 860.9625 MHz): the existing build produces ~3
#      sync hits/sec at threshold 10, but every NID decodes with random
#      NAC and a DUID histogram spread roughly uniformly across all 16
#      values (TSDU bucket = 4-5%, expected 100% on a control channel).
#      SDRTrunk on the same antenna decodes the same site cleanly using
#      its P25P1DemodulatorLSM, which is essentially RRC + Costas + slicer.
#
# So our current pipeline (C4FM-only) is:
#
#   DDC -> raw post-FIR (re, im)  ─┐
#                                  ├─> SymbolTimingRecovery
#   C4FMDemod -> diff_im  ─────────┘     ├ Gardner TED on diff_im
#                                        │ (still sample-rate, used for
#                                        │  clock recovery only)
#                                        └ Symbol-rate differential
#                                          z[k]*conj(z[k-1]) computed
#                                          on the latched IQ at the
#                                          decision point. Sign bits of
#                                          (diff_re, diff_im) -> dibit.
#                                          4 DSP48E1.
#
# `C4FMDemod` still computes the full complex differential at sample
# rate (used to be the slicer source); we keep it because diff_im is
# the natural FM cross-product and feeds Gardner TED. diff_re from
# C4FMDemod is no longer wired to anything.
#
# To add LSM support, two new blocks need to slot in between DDC and
# SymbolTimingRecovery:
#
#   1. RRC matched filter (P25 alpha=0.2, ~24 taps at 13 sps). Symmetric
#      FIR, ~12 DSP48E1. Real and imaginary share coefficients so total
#      cost is two 24-tap FIRs ~= 24 DSPs.
#
#   2. Costas loop carrier recovery: complex multiply IQ by NCO, slice,
#      compute phase error from sliced symbol, drive NCO via PI loop.
#      ~3 DSPs for the rotator + small NCO + loop filter.
#
# Alternative: move slicing entirely to the PS (Cortex-A9), use the
# existing IQ DMA infrastructure to stream raw post-DDC IQ to memory,
# and run RRC + Costas + slicer in p25-httpd. Easier to develop, easier
# to iterate, much easier to A/B test against SDRTrunk's reference.
# This is the currently-recommended path.
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
            + self.iq_dma.axi.ports()       # Phase 6C
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

        # ── Register crossbar ─────────────────────────────────────────
        # Address map (word-addressed via AXI4-Lite, 7-bit address):
        # Bank field is bits [5:3] of the word address (3 bits = 8
        # banks max). See doc/P25_ADDRESS_MAP.md for the canonical table.
        #
        #   word 0x00-0x07: control registers    (bits [5:3] == 000)
        #   word 0x08-0x0F: SDR/DDC registers    (bits [5:3] == 001)
        #   word 0x10-0x17: demod registers      (bits [5:3] == 010)
        #   word 0x18-0x1F: traffic registers    (bits [5:3] == 011)
        #   word 0x20-0x27: IQ DMA registers     (bits [5:3] == 100) [Phase 6C]
        #   word 0x28-0x3F: free for future banks
        address = Signal(self.axi4_awidth, reset_less=True)
        wdata = Signal(32, reset_less=True)
        addr_bank = self.axi4lite.address[3:6]  # bits [5:3]
        control_regs_select = (addr_bank == 0b000)
        sdr_regs_select = (addr_bank == 0b001)
        demod_regs_select = (addr_bank == 0b010)
        traffic_regs_select = (addr_bank == 0b011)
        iq_regs_select = (addr_bank == 0b100)       # Phase 6C
        m.d.s_axi_lite += [
            self.axi4lite.rdata.eq(self.control_registers.rdata
                                   | sdr_registers_cdc.i_rdata
                                   | demod_registers_cdc.i_rdata
                                   | traffic_registers_cdc.i_rdata
                                   | iq_registers_cdc.i_rdata),
            self.axi4lite.rdone.eq(self.control_registers.rdone
                                   | sdr_registers_cdc.i_rdone
                                   | demod_registers_cdc.i_rdone
                                   | traffic_registers_cdc.i_rdone
                                   | iq_registers_cdc.i_rdone),
            self.axi4lite.wdone.eq(self.control_registers.wdone
                                   | sdr_registers_cdc.i_wdone
                                   | demod_registers_cdc.i_wdone
                                   | traffic_registers_cdc.i_wdone
                                   | iq_registers_cdc.i_wdone),
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
