#
# Fishball P25 - Top-level IP core
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
from maia_hdl.dma import DmaStreamWrite
from maia_hdl.pluto_platform import PlutoPlatform
from maia_hdl.register import Access, Field, Registers, Register, RegisterMap

from .c4fm_demod import C4FMDemod
from .symbol_timing import SymbolTimingRecovery
from .dibit_packer import DibitPacker
from .config import P25Config
from . import configs

# IP core version
_version = '0.1.0'


class P25Core(Elaboratable):
    """Fishball P25 top-level IP core

    FPGA DSP pipeline for P25 Phase 1 trunking radio:
      AD9361 IQ -> DDC (tune + decimate) -> C4FM demod -> symbol timing
      -> dibit packer -> DMA to PS

    Reuses Maia SDR DDC, spectrometer (debug), recorder (debug),
    register infrastructure, and DMA modules.
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
        self.dibit_dma = DmaStreamWrite(
            config.dibit_dma_address,
            config.dibit_dma_address + config.dibit_dma_size,
            width=64, axi_awidth=32, name='m_axi_dibit')

        # ── Control channel demod registers (0x40) ────────────────────
        self.demod_registers = Registers(
            'demod',
            {
                0b00: Register('demod_status', [
                    Field('dibit_count', Access.R, 16, 0),
                    Field('demod_overflow', Access.Rsticky, 1, 0),
                ]),
                0b01: Register('demod_control', [
                    Field('start', Access.Wpulse, 1, 0),
                    Field('stop', Access.Wpulse, 1, 0),
                    Field('demod_enable', Access.RW, 1, 0),
                ]),
                0b10: Register('dibit_next_address', [
                    Field('next_address', Access.R, 32, 0),
                ]),
            },
            2)

        # ── Traffic channel DDC + demod chain ─────────────────────────
        self.traffic_ddc = DDC('clk3x')
        self.traffic_c4fm = C4FMDemod()
        self.traffic_timing = SymbolTimingRecovery(samples_per_symbol=13)
        self.traffic_packer = DibitPacker()
        self.traffic_dma = DmaStreamWrite(
            config.traffic_dma_address,
            config.traffic_dma_address + config.traffic_dma_size,
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
                    ]),
                0b100: Register(
                    'traffic_demod_control', [
                        Field('start', Access.Wpulse, 1, 0),
                        Field('stop', Access.Wpulse, 1, 0),
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
        self.register_map = RegisterMap({
            0x0: self.control_registers,
            0x08: self.sdr_registers,
            0x20: self.demod_registers,
            0x30: self.traffic_registers,
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
        m.submodules.demod_registers = s_axi_lite_renamer(
            self.demod_registers)

        # DDC output -> C4FM discriminator
        m.d.comb += [
            self.c4fm_demod.re_in.eq(self.ddc.re_out),
            self.c4fm_demod.im_in.eq(self.ddc.im_out),
            self.c4fm_demod.strobe_in.eq(self.ddc.strobe_out),
        ]

        # C4FM discriminator -> Symbol timing recovery
        m.d.comb += [
            self.symbol_timing.disc_in.eq(self.c4fm_demod.disc_out),
            self.symbol_timing.strobe_in.eq(self.c4fm_demod.strobe_out),
        ]

        # Symbol timing -> Dibit packer
        m.d.comb += [
            self.dibit_packer.dibit_in.eq(self.symbol_timing.dibit_out),
            self.dibit_packer.symbol_strobe.eq(
                self.symbol_timing.symbol_strobe),
        ]

        # Dibit packer -> DMA stream
        m.d.comb += [
            self.dibit_dma.stream_data.eq(self.dibit_packer.data_out),
            self.dibit_dma.stream_valid.eq(self.dibit_packer.data_valid),
            self.dibit_packer.stream_ready.eq(self.dibit_dma.stream_ready),
        ]

        # DMA start/stop from demod registers
        m.d.comb += [
            self.dibit_dma.start.eq(
                self.demod_registers['demod_control']['start']),
            self.dibit_dma.stop.eq(
                self.demod_registers['demod_control']['stop']),
        ]

        # DMA finished -> interrupt
        interrupts_reg = self.control_registers['interrupts']
        m.d.comb += [
            interrupts_reg['dibit_dma'].eq(self.dibit_dma.finished),
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
            self.demod_registers['dibit_next_address']['next_address'].eq(
                self.dibit_dma.next_address),
        ]

        # ── Traffic channel DDC + demod chain ─────────────────────────
        m.submodules.traffic_ddc = self.traffic_ddc
        m.submodules.traffic_c4fm = self.traffic_c4fm
        m.submodules.traffic_timing = self.traffic_timing
        m.submodules.traffic_packer = self.traffic_packer
        m.submodules.traffic_dma = self.traffic_dma
        m.submodules.traffic_registers = s_axi_lite_renamer(
            self.traffic_registers)

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

        # Traffic DDC -> C4FM demod -> symbol timing -> dibit packer -> DMA
        m.d.comb += [
            self.traffic_c4fm.re_in.eq(self.traffic_ddc.re_out),
            self.traffic_c4fm.im_in.eq(self.traffic_ddc.im_out),
            self.traffic_c4fm.strobe_in.eq(self.traffic_ddc.strobe_out),
            self.traffic_timing.disc_in.eq(self.traffic_c4fm.disc_out),
            self.traffic_timing.strobe_in.eq(self.traffic_c4fm.strobe_out),
            self.traffic_packer.dibit_in.eq(self.traffic_timing.dibit_out),
            self.traffic_packer.symbol_strobe.eq(
                self.traffic_timing.symbol_strobe),
            self.traffic_dma.stream_data.eq(self.traffic_packer.data_out),
            self.traffic_dma.stream_valid.eq(self.traffic_packer.data_valid),
            self.traffic_packer.stream_ready.eq(
                self.traffic_dma.stream_ready),
            self.traffic_dma.start.eq(
                self.traffic_registers['traffic_demod_control']['start']),
            self.traffic_dma.stop.eq(
                self.traffic_registers['traffic_demod_control']['stop']),
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
            self.traffic_registers['traffic_next_address']['next_address'].eq(
                self.traffic_dma.next_address),
        ]

        # ── Register crossbar ─────────────────────────────────────────
        # Address map (word-addressed via AXI4-Lite, 7-bit address):
        #   0x00-0x03: control registers    (bits [4:3] == 00)
        #   0x08-0x0F: SDR/DDC registers    (bits [4:3] == 01)
        #   0x10-0x17: demod registers      (bits [4:3] == 10)
        #   0x18-0x1F: traffic registers    (bits [4:3] == 11)
        address = Signal(self.axi4_awidth, reset_less=True)
        wdata = Signal(32, reset_less=True)
        addr_bank = self.axi4lite.address[3:5]  # bits [4:3]
        control_regs_select = (addr_bank == 0b00)
        sdr_regs_select = (addr_bank == 0b01)
        demod_regs_select = (addr_bank == 0b10)
        traffic_regs_select = (addr_bank == 0b11)
        m.d.s_axi_lite += [
            self.axi4lite.rdata.eq(self.control_registers.rdata
                                   | sdr_registers_cdc.i_rdata
                                   | self.demod_registers.rdata
                                   | self.traffic_registers.rdata),
            self.axi4lite.rdone.eq(self.control_registers.rdone
                                   | sdr_registers_cdc.i_rdone
                                   | self.demod_registers.rdone
                                   | self.traffic_registers.rdone),
            self.axi4lite.wdone.eq(self.control_registers.wdone
                                   | sdr_registers_cdc.i_wdone
                                   | self.demod_registers.wdone
                                   | self.traffic_registers.wdone),
            self.control_registers.ren.eq(
                self.axi4lite.ren & control_regs_select),
            self.control_registers.wstrobe.eq(
                Mux(control_regs_select, self.axi4lite.wstrobe, 0)),
            sdr_registers_cdc.i_ren.eq(
                self.axi4lite.ren & sdr_regs_select),
            sdr_registers_cdc.i_wstrobe.eq(
                Mux(sdr_regs_select, self.axi4lite.wstrobe, 0)),
            self.demod_registers.ren.eq(
                self.axi4lite.ren & demod_regs_select),
            self.demod_registers.wstrobe.eq(
                Mux(demod_regs_select, self.axi4lite.wstrobe, 0)),
            self.traffic_registers.ren.eq(
                self.axi4lite.ren & traffic_regs_select),
            self.traffic_registers.wstrobe.eq(
                Mux(traffic_regs_select, self.axi4lite.wstrobe, 0)),
            address.eq(self.axi4lite.address),
            wdata.eq(self.axi4lite.wdata),
        ]
        m.d.comb += [
            self.control_registers.address.eq(address),
            self.control_registers.wdata.eq(wdata),
            sdr_registers_cdc.i_address.eq(address),
            sdr_registers_cdc.i_wdata.eq(wdata),
            self.demod_registers.address.eq(address),
            self.demod_registers.wdata.eq(wdata),
            self.traffic_registers.address.eq(address),
            self.traffic_registers.wdata.eq(wdata),
        ]

        # ── Registers sync domain ────────────────────────────────────
        m.d.comb += [
            self.sdr_registers.ren.eq(sdr_registers_cdc.o_ren),
            self.sdr_registers.wstrobe.eq(sdr_registers_cdc.o_wstrobe),
            self.sdr_registers.address.eq(sdr_registers_cdc.o_address),
            self.sdr_registers.wdata.eq(sdr_registers_cdc.o_wdata),
            sdr_registers_cdc.o_rdone.eq(self.sdr_registers.rdone),
            sdr_registers_cdc.o_wdone.eq(self.sdr_registers.wdone),
            sdr_registers_cdc.o_rdata.eq(self.sdr_registers.rdata),
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
