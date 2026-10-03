#
# Fishball P25 - the lane ring core (doc/changes/079_general_radio_core.md, step 3a).
#
# The AD9361's samples go to N lanes, each a DDC (tune and decimate to 50 kSPS) whose output is
# cut into tagged packets (lane_packetizer.py); one ring DMA carries every lane's packets to the
# PS, where all demodulation runs. Beside the lanes: the wideband spectrometer and the raw IQ
# capture ring. The register bridge answers every access.
#
#   rxiq_cdc -> input register -+-> lane DDC 0..N-1 -> packetizers -> LaneRing -> lanes_dma
#                               +-> spectrometer -> wideband_spec_dma
#                               +-> IQPacker -> wideband_iq_dma
#
# Registers (byte offsets, 32-byte banks; doc/changes/079 "Registers"):
#   0x000           control (AXI-Lite domain): product id, version, control, interrupts,
#                   capabilities
#   0x020 * (1+i)   lane i: DDC coefficients and stages, NCO, lane control (enable, tag), status
#   then            lanes ring, spectrometer, capture
#
# SPDX-License-Identifier: MIT
#

import argparse

from amaranth import *
from amaranth.lib.cdc import FFSynchronizer, PulseSynchronizer
import amaranth.back.verilog

from maia_hdl.cdc import RegisterCDC, RxIQCDC
from maia_hdl.clknx import ClkNxCommonEdge
from maia_hdl.dma import DmaStreamRingWrite
from maia_hdl.pluto_platform import PlutoPlatform
from maia_hdl.register import Access, Field, Registers, Register, RegisterMap
from maia_hdl.spectrometer import Spectrometer

from .axil_bridge import AnsweringRegisterBridge
from .config import P25Config
from .iq_packer import IQPacker
from .lane_packetizer import HEADER_WORDS, LanePacketizer, PACKET_WORDS
from .lane_ring import LaneRing
from .p25ddc import P25DDC
from . import configs

# IP core version. 1.0.0: the lane ring core (doc/changes/079).
_version = '1.0.0'
PRODUCT_ID = 0x72616431   # "rad1"

# Word address bits of the register bus (a 1 KB window), and of one bank (8 words).
ADDRESS_WIDTH = 8
BANK_WORDS_LOG2 = 3
# AXI-Lite cycles after `sdr_reset` clears before the sync-domain banks are claimed (their reset
# synchronisers release a few cycles later).
LIVE_DELAY = 16


class P25Core(Elaboratable):
    """The lane ring core.

    Parameters
    ----------
    config : P25Config
    sim : bool
        For simulation: the sample input is ``sim_re``/``sim_im``/``sim_strobe`` in ``sync``
        instead of the FIFO-based input crossing, which the simulator cannot run.
    """
    def __init__(self, config=P25Config(), *, sim=False):
        config.validate()
        self.config = config
        self.sim = sim
        self.lanes = config.lanes

        self.s_axi_lite = ClockDomain()
        self.sampling = ClockDomain()
        self.sync = ClockDomain()
        self.clk3x = ClockDomain()
        self.clk2x = ClockDomain()

        self.axi4lite = AnsweringRegisterBridge(ADDRESS_WIDTH, name='s_axi_lite')

        self.control_registers = Registers('control', {
            0b000: Register('product_id', [Field('product_id', Access.R, 32, PRODUCT_ID)]),
            0b001: Register('version', [
                Field('bugfix', Access.R, 8, int(_version.split('.')[2])),
                Field('minor', Access.R, 8, int(_version.split('.')[1])),
                Field('major', Access.R, 8, int(_version.split('.')[0])),
                Field('platform', Access.R, 8, config.platform),
            ]),
            0b010: Register('control', [Field('sdr_reset', Access.RW, 1, 1)]),
            0b011: Register('interrupts', [
                Field('lanes_ring', Access.Rsticky, 1, 0),
                Field('spectrum', Access.Rsticky, 1, 0),
                Field('capture', Access.Rsticky, 1, 0),
            ], interrupt=True),
            0b100: Register('capabilities', [
                Field('lanes', Access.R, 4, self.lanes),
                Field('packet_words_log2', Access.R, 4, PACKET_WORDS.bit_length() - 1),
                Field('header_words', Access.R, 4, HEADER_WORDS),
                Field('spectrum', Access.R, 1, 1),
                Field('capture', Access.R, 1, 1),
            ]),
        }, BANK_WORDS_LOG2)

        self.ddcs = [P25DDC('clk3x') for _ in range(self.lanes)]
        self.packetizers = [LanePacketizer(i) for i in range(self.lanes)]
        self.lane_registers = [self._lane_bank(i) for i in range(self.lanes)]

        self.lane_ring = LaneRing(self.packetizers)
        self.lanes_dma = DmaStreamRingWrite(
            config.lanes_dma_address, config.lanes_dma_num_buffers_log2,
            config.lanes_dma_buffer_size, width=64, axi_awidth=32, name='m_axi_lanes')
        self.ring_registers = Registers('lanes_ring', {
            0b000: Register('lanes_ring_control', [Field('enable', Access.RW, 1, 0)]),
            0b001: Register('lanes_ring_status', [
                Field('last_buffer', Access.R, config.lanes_dma_num_buffers_log2, -1),
            ]),
            0b010: Register('lanes_ring_next_address', [Field('next_address', Access.R, 32, 0)]),
            # Reading the low word latches the high word, so the two halves are one count.
            0b011: Register('sample_count_lo', [Field('count', Access.R, 32, 0)]),
            0b100: Register('sample_count_hi', [Field('count', Access.R, 32, 0)]),
            0b101: Register('adc_clips', [Field('count', Access.R, 32, 0)]),
        }, BANK_WORDS_LOG2)

        self.wideband_spec = Spectrometer(
            config.wideband_spec_dma_address, config.wideband_spec_dma_num_buffers_log2,
            dma_name='m_axi_wideband_spec', domain_2x='clk2x', domain_3x='clk3x')
        self.spec_registers = Registers('spectrometer', {
            0b000: Register('spec_control', [
                Field('spec_enable', Access.RW, 1, 0),
                Field('spec_peak_detect', Access.RW, 1, 0),
                Field('spec_abort', Access.Wpulse, 1, 0),
                Field('spec_num_integrations', Access.RW, self.wideband_spec.nint_width, -1),
            ]),
            0b001: Register('spec_status', [
                Field('spec_overflow', Access.Rsticky, 1, 0),
                Field('spec_last_buffer', Access.R, len(self.wideband_spec.last_buffer), -1),
            ]),
            0b010: Register('spec_next_address', [Field('next_address', Access.R, 32, 0)]),
        }, BANK_WORDS_LOG2)

        self.wideband_iq_packer = IQPacker()
        self.wideband_iq_dma = DmaStreamRingWrite(
            config.wideband_iq_dma_address, config.wideband_iq_dma_num_buffers_log2,
            config.wideband_iq_dma_buffer_size, width=64, axi_awidth=32,
            name='m_axi_wideband_iq')
        self.capture_registers = Registers('wideband_iq', {
            0b000: Register('wideband_iq_dma_status', [
                Field('wideband_iq_overflow', Access.Rsticky, 1, 0),
                Field('last_buffer', Access.R, config.wideband_iq_dma_num_buffers_log2, -1),
            ]),
            0b001: Register('wideband_iq_dma_control', [
                Field('wideband_iq_enable', Access.RW, 1, 0),
            ]),
            0b010: Register('wideband_iq_next_address', [Field('next_address', Access.R, 32, 0)]),
        }, BANK_WORDS_LOG2)

        # Banks in address order: control, the lanes, then the ring, spectrum and capture.
        self.sync_banks = self.lane_registers + [
            self.ring_registers, self.spec_registers, self.capture_registers]
        self.bank_offsets = {id(bank): 0x20 * (1 + i) for i, bank in enumerate(self.sync_banks)}
        self.register_map = RegisterMap(
            {0x00: self.control_registers,
             **{0x20 * (1 + i): bank for i, bank in enumerate(self.sync_banks)}},
            {
                'vendor': 'Andy Lee',
                'vendorID': 'fishball-p25',
                'name': 'Radio Core',
                'series': 'Fishball',
                'version': _version,
                'description': f'Fishball radio core: lanes, spectrum, capture '
                               f'(platform {config.platform})',
                'licenseText': 'SPDX-License-Identifier: MIT',
            })

        self.iq_in_width = 12
        self.re_in = Signal(self.iq_in_width)
        self.im_in = Signal(self.iq_in_width)
        self.interrupt_out = Signal()
        if sim:
            self.sim_re = Signal(signed(12))
            self.sim_im = Signal(signed(12))
            self.sim_strobe = Signal()

    @staticmethod
    def _lane_bank(i):
        """Lane i: the DDC registers at the 0.3.0 offsets, then the lane's control and status."""
        p = f'lane{i}_'
        return Registers(f'lane{i}', {
            0b000: Register(p + 'ddc_coeff_addr', [Field('coeff_waddr', Access.RW, 10, 0)]),
            0b010: Register(p + 'ddc_coeff', [
                Field('coeff_wren', Access.Wpulse, 1, 0),
                Field('coeff_wdata', Access.RW, 18, 0),
            ]),
            0b011: Register(p + 'ddc_decimation', [
                Field('decimation1', Access.RW, 7, 0),
                Field('decimation2', Access.RW, 6, 0),
                Field('decimation3', Access.RW, 7, 0),
            ]),
            0b100: Register(p + 'ddc_frequency', [Field('frequency', Access.RW, 28, 0)]),
            0b101: Register(p + 'ddc_control', [
                Field('operations_minus_one1', Access.RW, 7, 0),
                Field('operations_minus_one2', Access.RW, 6, 0),
                Field('operations_minus_one3', Access.RW, 7, 0),
                Field('odd_operations1', Access.RW, 1, 0),
                Field('odd_operations3', Access.RW, 1, 0),
                Field('bypass2', Access.RW, 1, 0),
                Field('bypass3', Access.RW, 1, 0),
                Field('enable_input', Access.RW, 1, 0),
            ]),
            0b110: Register(p + 'control', [
                Field('enable', Access.RW, 1, 0),
                Field('reserved', Access.R, 15, 0),
                Field('tag', Access.RW, 16, 0),
            ]),
            # `lost` alone in its word: reading it clears it.
            0b111: Register(p + 'status', [Field('lost', Access.Rsticky, 1, 0)]),
        }, BANK_WORDS_LOG2)

    def ports(self):
        return (
            self.axi4lite.axi.ports()
            + self.lanes_dma.axi.ports()
            + self.wideband_spec.dma.axi.ports()
            + self.wideband_iq_dma.axi.ports()
            + [
                self.re_in, self.im_in, self.interrupt_out,
                self.s_axi_lite.clk, self.s_axi_lite.rst,
                self.sampling.clk, self.sync.clk, self.sync.rst,
                self.clk3x.clk, self.clk2x.clk,
            ]
        )

    def svd(self):
        return self.register_map.svd()

    def elaborate(self, platform):
        m = Module()
        m.domains += [self.s_axi_lite, self.sampling, self.sync, self.clk3x, self.clk2x]
        cfg = self.config

        s_axi_lite = DomainRenamer({'sync': 's_axi_lite'})
        m.submodules.axi4lite = s_axi_lite(self.axi4lite)
        m.submodules.control_registers = s_axi_lite(self.control_registers)
        sdr_reset = self.control_registers['control']['sdr_reset']

        # ── Input: the AD9361's samples, registered once for every consumer ──────────────
        rx_re = Signal(signed(12))
        rx_im = Signal(signed(12))
        rx_strobe = Signal()
        if self.sim:
            m.d.comb += [rx_re.eq(self.sim_re), rx_im.eq(self.sim_im),
                         rx_strobe.eq(self.sim_strobe)]
        else:
            m.submodules.rxiq_cdc = rxiq_cdc = RxIQCDC('sampling', 'sync', self.iq_in_width)
            m.d.comb += [
                rxiq_cdc.re_in.eq(self.re_in),
                rxiq_cdc.im_in.eq(self.im_in),
                rxiq_cdc.reset.eq(sdr_reset),
                rx_re.eq(rxiq_cdc.re_out.as_signed()),
                rx_im.eq(rxiq_cdc.im_out.as_signed()),
                rx_strobe.eq(rxiq_cdc.strobe_out),
            ]
        in_re = Signal(signed(12), reset_less=True)
        in_im = Signal(signed(12), reset_less=True)
        in_strobe = Signal()
        m.d.sync += [in_re.eq(rx_re), in_im.eq(rx_im), in_strobe.eq(rx_strobe)]

        # The AD9361 sample count and the samples at full scale, for the packet headers.
        sample_count = Signal(64)
        adc_clips = Signal(32)
        with m.If(in_strobe):
            m.d.sync += sample_count.eq(sample_count + 1)
            with m.If((in_re == 2047) | (in_re == -2048) | (in_im == 2047) | (in_im == -2048)):
                m.d.sync += adc_clips.eq(adc_clips + 1)

        m.submodules.common_edge_3x = common_edge_3x = ClkNxCommonEdge('sync', 'clk3x', 3)
        m.submodules.common_edge_2x = common_edge_2x = ClkNxCommonEdge('sync', 'clk2x', 2)

        # ── Lanes ────────────────────────────────────────────────────────────────────────
        for i, (ddc, pk, regs) in enumerate(zip(self.ddcs, self.packetizers, self.lane_registers)):
            setattr(m.submodules, f'lane{i}_ddc', ddc)
            setattr(m.submodules, f'lane{i}_packetizer', pk)
            p = f'lane{i}_'
            ctrl = regs[p + 'ddc_control']
            dec = regs[p + 'ddc_decimation']
            m.d.comb += [
                ddc.common_edge.eq(common_edge_3x.common_edge),
                ddc.enable_input.eq(ctrl['enable_input']),
                ddc.frequency.eq(regs[p + 'ddc_frequency']['frequency']),
                ddc.coeff_waddr.eq(regs[p + 'ddc_coeff_addr']['coeff_waddr']),
                ddc.coeff_wren.eq(regs[p + 'ddc_coeff']['coeff_wren']),
                ddc.coeff_wdata.eq(regs[p + 'ddc_coeff']['coeff_wdata']),
                ddc.decimation1.eq(dec['decimation1']),
                ddc.decimation2.eq(dec['decimation2']),
                ddc.decimation3.eq(dec['decimation3']),
                ddc.bypass2.eq(ctrl['bypass2']),
                ddc.bypass3.eq(ctrl['bypass3']),
                ddc.operations_minus_one1.eq(ctrl['operations_minus_one1']),
                ddc.operations_minus_one2.eq(ctrl['operations_minus_one2']),
                ddc.operations_minus_one3.eq(ctrl['operations_minus_one3']),
                ddc.odd_operations1.eq(ctrl['odd_operations1']),
                ddc.odd_operations3.eq(ctrl['odd_operations3']),
                ddc.strobe_in.eq(in_strobe),
                ddc.re_in.eq(in_re),
                ddc.im_in.eq(in_im),

                pk.re_in.eq(ddc.re_out),
                pk.im_in.eq(ddc.im_out),
                pk.strobe_in.eq(ddc.strobe_out),
                pk.enable.eq(regs[p + 'control']['enable']),
                pk.tag.eq(regs[p + 'control']['tag']),
                pk.nco.eq(regs[p + 'ddc_frequency']['frequency']),
                pk.sample_index.eq(sample_count),
                pk.adc_clips.eq(adc_clips),
                regs[p + 'status']['lost'].eq(pk.lost),
            ]

        # ── The lane ring ────────────────────────────────────────────────────────────────
        m.submodules.lane_ring = self.lane_ring
        m.submodules.lanes_dma = self.lanes_dma
        ring = self.ring_registers
        count_hi = Signal(32)
        m.d.comb += [
            self.lanes_dma.stream_data.eq(self.lane_ring.stream_data),
            self.lanes_dma.stream_valid.eq(self.lane_ring.stream_valid),
            self.lane_ring.stream_ready.eq(self.lanes_dma.stream_ready),
            self.lanes_dma.enable.eq(self.lane_ring.dma_enable),
            self.lane_ring.aw_accepted.eq(self.lanes_dma.axi.aw_handshake()),
            self.lane_ring.enable.eq(ring['lanes_ring_control']['enable']),
            ring['lanes_ring_status']['last_buffer'].eq(self.lanes_dma.last_buffer),
            ring['lanes_ring_next_address']['next_address'].eq(self.lanes_dma.axi.awaddr),
            ring['sample_count_lo']['count'].eq(sample_count[:32]),
            ring['sample_count_hi']['count'].eq(count_hi),
            ring['adc_clips']['count'].eq(adc_clips),
        ]
        with m.If(ring.ren & (ring.address[:BANK_WORDS_LOG2] == 0b011)):
            m.d.sync += count_hi.eq(sample_count[32:])

        # ── Spectrometer ─────────────────────────────────────────────────────────────────
        m.submodules.wideband_spec = self.wideband_spec
        spec_ctrl = self.spec_registers['spec_control']
        spec_stat = self.spec_registers['spec_status']
        spec_strobe = Signal()
        m.d.sync += spec_strobe.eq(rx_strobe & spec_ctrl['spec_enable'])
        m.d.comb += [
            self.wideband_spec.re_in.eq(in_re),
            self.wideband_spec.im_in.eq(in_im),
            self.wideband_spec.strobe_in.eq(spec_strobe),
            self.wideband_spec.common_edge_2x.eq(common_edge_2x.common_edge),
            self.wideband_spec.common_edge_3x.eq(common_edge_3x.common_edge),
            self.wideband_spec.number_integrations.eq(spec_ctrl['spec_num_integrations']),
            self.wideband_spec.peak_detect.eq(spec_ctrl['spec_peak_detect']),
            self.wideband_spec.abort.eq(spec_ctrl['spec_abort']),
            spec_stat['spec_last_buffer'].eq(self.wideband_spec.last_buffer),
            self.spec_registers['spec_next_address']['next_address'].eq(
                self.wideband_spec.dma.axi.awaddr),
        ]

        # ── Raw IQ capture ───────────────────────────────────────────────────────────────
        m.submodules.wideband_iq_packer = self.wideband_iq_packer
        m.submodules.wideband_iq_dma = self.wideband_iq_dma
        cap = self.capture_registers
        m.d.comb += [
            self.wideband_iq_packer.re_in.eq(in_re),
            self.wideband_iq_packer.im_in.eq(in_im),
            self.wideband_iq_packer.strobe_in.eq(in_strobe),
            self.wideband_iq_dma.stream_data.eq(self.wideband_iq_packer.data_out),
            self.wideband_iq_dma.stream_valid.eq(self.wideband_iq_packer.data_valid),
            self.wideband_iq_packer.stream_ready.eq(self.wideband_iq_dma.stream_ready),
            self.wideband_iq_dma.enable.eq(cap['wideband_iq_dma_control']['wideband_iq_enable']),
            cap['wideband_iq_dma_status']['wideband_iq_overflow'].eq(
                self.wideband_iq_packer.overflow),
            cap['wideband_iq_dma_status']['last_buffer'].eq(self.wideband_iq_dma.last_buffer),
            cap['wideband_iq_next_address']['next_address'].eq(self.wideband_iq_dma.axi.awaddr),
        ]

        # ── Interrupts: each source's pulse into the AXI-Lite domain ─────────────────────
        interrupts = self.control_registers['interrupts']
        for name, pulse in [('lanes_ring', self.lanes_dma.interrupt),
                            ('spectrum', self.wideband_spec.interrupt_out),
                            ('capture', self.wideband_iq_dma.interrupt)]:
            sync = PulseSynchronizer('sync', 's_axi_lite')
            setattr(m.submodules, f'{name}_irq_sync', sync)
            m.d.comb += [sync.i.eq(pulse), interrupts[name].eq(sync.o)]
        m.d.comb += self.interrupt_out.eq(interrupts.interrupt)

        # ── Register bus: the control bank here, the others across RegisterCDCs ──────────
        # The sync-domain banks are claimed only while their domain has been out of reset for a
        # while; any other address is answered by the bridge. `sdr_reset` comes through a
        # synchroniser like every other consumer's (the project waives timing from its flop).
        live = Signal()
        live_count = Signal(range(LIVE_DELAY + 1))
        reset_seen = Signal()
        m.submodules.s_axi_lite_sdr_reset = FFSynchronizer(
            sdr_reset, reset_seen, o_domain='s_axi_lite', init=1)
        with m.If(reset_seen):
            m.d.s_axi_lite += [live.eq(0), live_count.eq(0)]
        with m.Elif(live_count < LIVE_DELAY):
            m.d.s_axi_lite += live_count.eq(live_count + 1)
        with m.Else():
            m.d.s_axi_lite += live.eq(1)

        bus = self.axi4lite
        bank = bus.address[BANK_WORDS_LOG2:]
        control_sel = bank == 0
        selects = [bank == (1 + i) for i in range(len(self.sync_banks))]
        m.d.comb += bus.claimed.eq(control_sel | (live & Cat(*selects).any()))

        rdata = self.control_registers.rdata
        rdone = self.control_registers.rdone
        wdone = self.control_registers.wdone
        m.d.comb += [
            self.control_registers.ren.eq(bus.ren & control_sel),
            self.control_registers.wstrobe.eq(Mux(control_sel, bus.wstrobe, 0)),
            self.control_registers.address.eq(bus.address),
            self.control_registers.wdata.eq(bus.wdata),
        ]
        for i, (regs, sel) in enumerate(zip(self.sync_banks, selects)):
            setattr(m.submodules, f'{regs.name}_registers', regs)
            cdc = RegisterCDC('s_axi_lite', 'sync', regs.aw)
            setattr(m.submodules, f'{regs.name}_registers_cdc', cdc)
            m.d.comb += [
                cdc.i_ren.eq(bus.ren & sel),
                cdc.i_wstrobe.eq(Mux(sel, bus.wstrobe, 0)),
                cdc.i_address.eq(bus.address),
                cdc.i_wdata.eq(bus.wdata),
                regs.ren.eq(cdc.o_ren),
                regs.wstrobe.eq(cdc.o_wstrobe),
                regs.address.eq(cdc.o_address),
                regs.wdata.eq(cdc.o_wdata),
                cdc.o_rdone.eq(regs.rdone),
                cdc.o_wdone.eq(regs.wdone),
                cdc.o_rdata.eq(regs.rdata),
            ]
            rdata = rdata | cdc.i_rdata
            rdone = rdone | cdc.i_rdone
            wdone = wdone | cdc.i_wdone
        m.d.s_axi_lite += [bus.rdata.eq(rdata), bus.rdone.eq(rdone), bus.wdone.eq(wdone)]

        # ── Resets: every processing domain follows `sdr_reset` ──────────────────────────
        for internal in ['sync', 'clk3x', 'clk2x', 'sampling']:
            setattr(m.submodules, f'{internal}_rst', FFSynchronizer(
                sdr_reset, ResetSignal(internal), o_domain=internal, init=1))
        return m


def write_svd(path):
    with open(path, 'wb') as f:
        f.write(P25Core().svd())


def parse_args():
    parser = argparse.ArgumentParser(description='Generate the Fishball P25 IP core Verilog')
    parser.add_argument('--config', default='default',
                        help='P25 configuration name [default=%(default)r]')
    parser.add_argument('output_file', help='Output Verilog file')
    return parser.parse_args()


def main():
    args = parse_args()
    config = getattr(configs, args.config)()
    top = P25Core(config)
    with open(args.output_file, 'w') as f:
        f.write(amaranth.back.verilog.convert(top, platform=PlutoPlatform(), ports=top.ports()))


if __name__ == '__main__':
    main()
