#
# Fishball hardware validation (hwval) - Top-level IP core
#
# Validation bitstream core (doc/HW_VALIDATION_SUITE.md section 6):
#
#   s_axi_lite (FCLK0 100 MHz) -- AxiLiteRegisterFile (regmap.py)
#     |  config + commands -> DomainCrossing -> sync, clk2x, sampling
#     |  status  <- Snapshot (SNAP_REQ/SNAP_ACK) <- sync, clk2x, sampling
#     |  live    <- GrayCounterSync (RINGV2_COMMITTED_BURSTS), FFSynchronizer
#     |  census, event FIFO read side and IRQ logic live in s_axi_lite
#
#   re_in/im_in/valid_in (sampling) -> IngestMonitor
#                                   \-> IngestCDC -> sync
#       sync: live IQ (12 -> 16 bit sign extension)
#         +-> SamplePattern (legacy src) -> LegacyRing -> m_axi_legacy (HP1)
#         +-> IQ word packer -> WordPattern (ringv2 src) -> RingWriterV2
#                                                        -> m_axi_ringv2 (HP1)
#   clk2x (125 MHz): AxiMemTester x2 -> m_axi_mt0 (HP0), m_axi_mt1 (HP3)
#   ctrl_out (async) -> EventRecorder (sync -> s_axi_lite FIFO)
#   every clock -> ClockCensus (ref s_axi_lite; ad_clkout sampled in clk3x)
#
# CORE_RESET (s_axi_lite register, default 0) drives the resets of all the
# other domains through FFSynchronizers (like p25_core's sdr_reset), and
# the FIFO18E1 resets of IngestCDC and EventRecorder together with a
# power-on pulse. Pulse CORE_RESET once the sampling clock runs (AD9361
# configured) so the IngestCDC FIFO18E1 sees a reset with both clocks
# running.
#
# SPDX-License-Identifier: MIT
#

import argparse

from amaranth import *
from amaranth.lib.cdc import FFSynchronizer
import amaranth.back.verilog

from maia_hdl.pluto_platform import PlutoPlatform

from .axil_regs import AxiLiteRegisterFile
from .cdc_util import (
    DomainCrossing, GrayCounterSync, Snapshot, config_sync, ff_sync,
    pulse_sync)
from .config import HwvalConfig
from . import config as config_mod
from .regmap import (
    CENSUS_CLOCKS, MARKDOWN_PREAMBLE, build_register_table)

from .axi_memtest import AxiMemTester
from .clock_census import ClockCensus
from .dna import DnaReader
from .event_rec import EventRecorder
from .ingest import IngestCDC, IngestMonitor
from .legacy_ring import LegacyRing
from .pattern import RateGen, SamplePattern, WordPattern
from .ring_v2 import RingWriterV2

# IP core version (see HwvalConfig.version)
_version = HwvalConfig().version_str


class HwvalCore(Elaboratable):
    """Fishball hwval top-level IP core

    Parameters
    ----------
    config : HwvalConfig
        Core configuration.
    sim : bool
        Use simulation models for the Xilinx FIFO primitives (IngestCDC,
        EventRecorder) so the core can be simulated with pysim.

    Attributes
    ----------
    regs : AxiLiteRegisterFile
        Register file; ``regs.axi`` is the ``s_axi_lite`` port.
    re_in, im_in : Signal(12), in (sampling)
    valid_in : Signal(), in (sampling)
    ctrl_out : Signal(8), in (asynchronous, AD9361 CTRL_OUT)
    ad_clkout : Signal(), in (asynchronous, AD9361 CLK_OUT)
    interrupt_out : Signal(), out (s_axi_lite, level)
    """
    # Domains whose reset is driven from CORE_RESET
    INTERNAL_DOMAINS = ['sync', 'clk2x', 'clk3x', 'sampling', 'lclk',
                        'fclk1', 'y1']
    POR_CYCLES = 255

    def __init__(self, config=None, *, sim=False):
        if config is None:
            config = HwvalConfig()
        config.validate()
        self.config = config
        self.sim = sim
        self.table = build_register_table(config)

        # Clock domains (Verilog ports: s_axi_lite_clk, s_axi_lite_rst,
        # clk, rst (out), clk2x_clk, clk3x_clk, sampling_clk, lclk_clk,
        # fclk1_clk, y1_clk). A domain called 'sync' is declared explicitly
        # because its reset is driven internally, see
        # https://github.com/amaranth-lang/amaranth/issues/1506
        self.s_axi_lite = ClockDomain()
        self.sync = ClockDomain()
        self.clk2x = ClockDomain()
        self.clk3x = ClockDomain()
        self.sampling = ClockDomain()
        self.lclk = ClockDomain()
        self.fclk1 = ClockDomain()
        self.y1 = ClockDomain()

        self.regs = AxiLiteRegisterFile(self.table, name='s_axi_lite')

        # ── Submodules ──────────────────────────────────────────
        self.ringv2 = RingWriterV2(
            name='m_axi_ringv2', fifo_depth=config.ringv2_fifo_depth,
            max_outstanding=config.ringv2_max_outstanding)
        self.legacy = LegacyRing(
            base=config.legacy_base,
            num_buffers_log2=config.legacy_num_buffers_log2,
            buffer_size=config.legacy_buffer_size, name='m_axi_legacy')
        self.mt0 = AxiMemTester(
            name='m_axi_mt0', max_outstanding=config.memtest_max_outstanding)
        self.mt1 = AxiMemTester(
            name='m_axi_mt1', max_outstanding=config.memtest_max_outstanding)
        self.census = ClockCensus(
            [(name, domain) for _, name, domain, _ in CENSUS_CLOCKS],
            sampled=('clkout',), ref_domain='s_axi_lite',
            sampler_domain='clk3x')
        self.ingest = IngestMonitor()
        self.ingest_cdc = IngestCDC('sampling', 'sync', 12, sim=sim)
        self.evt = EventRecorder(heartbeat_log2=config.evt_heartbeat_log2,
                                 sim=sim)
        self.dna = DnaReader(sim=sim)
        self.ringv2_rate = RateGen()
        self.ringv2_pattern = WordPattern()
        self.legacy_rate = RateGen()
        self.legacy_pattern = SamplePattern()

        # ── I/O ─────────────────────────────────────────────────
        self.iq_in_width = 12
        self.re_in = Signal(self.iq_in_width)
        self.im_in = Signal(self.iq_in_width)
        self.valid_in = Signal()
        self.ctrl_out = Signal(8)
        self.ad_clkout = Signal()
        self.interrupt_out = Signal()

    def ports(self):
        return (
            self.regs.axi.ports()
            + self.ringv2.axi.ports()
            + self.legacy.axi.ports()
            + self.mt0.axi.ports()
            + self.mt1.axi.ports()
            + [
                self.re_in,
                self.im_in,
                self.valid_in,
                self.ctrl_out,
                self.ad_clkout,
                self.interrupt_out,
                self.s_axi_lite.clk,
                self.s_axi_lite.rst,
                self.sync.clk,
                self.sync.rst,
                self.clk2x.clk,
                self.clk3x.clk,
                self.sampling.clk,
                self.lclk.clk,
                self.fclk1.clk,
                self.y1.clk,
            ]
        )

    def svd(self):
        return self.table.to_svd()

    def elaborate(self, platform):
        m = Module()
        m.domains += [
            self.s_axi_lite,
            self.sync,
            self.clk2x,
            self.clk3x,
            self.sampling,
            self.lclk,
            self.fclk1,
            self.y1,
        ]
        table = self.table
        regs = self.regs
        axil = m.d.s_axi_lite
        m.submodules.regs = DomainRenamer({'sync': 's_axi_lite'})(regs)

        driven = set()

        def drive(name, value):
            """Drive an ro register (records it for the completeness
            check at the end of elaborate)."""
            assert table[name].access == 'ro', name
            assert name not in driven, f'{name} driven twice'
            driven.add(name)
            m.d.comb += regs[name].eq(value)

        def drive64(name, value):
            value = Value.cast(value)
            ext = Signal(signed(64) if value.shape().signed else 64,
                         name=f'{name.lower()}_ext')
            m.d.comb += ext.eq(value)
            drive(f'{name}_LO', ext[:32])
            drive(f'{name}_HI', ext[32:])

        # ── Resets ──────────────────────────────────────────────
        core_reset = regs.field('CORE_RESET', 'reset')
        for d in self.INTERNAL_DOMAINS:
            m.submodules[f'{d}_rst'] = FFSynchronizer(
                core_reset, ResetSignal(d), o_domain=d, init=1)
        # FIFO18E1 reset (IngestCDC, EventRecorder): power-on pulse after
        # the AXI-Lite reset, or CORE_RESET.
        por_count = Signal(range(self.POR_CYCLES + 1))
        with m.If(por_count != self.POR_CYCLES):
            axil += por_count.eq(por_count + 1)
        fifo_reset = Signal(init=1)
        axil += fifo_reset.eq(core_reset | (por_count != self.POR_CYCLES))

        # ── Configuration / command crossings ───────────────────
        # One ordered crossing per destination domain carries all the
        # configuration registers and command bits of that domain (see
        # DomainCrossing): a command never arrives before configuration
        # written ahead of it, and no write is delivered in an earlier
        # transfer than a write that preceded it.
        xings = {}
        for d in ['sync', 'clk2x', 'sampling']:
            xings[d] = DomainCrossing('s_axi_lite', d, name=f'xing_{d}')
            m.submodules[f'xing_{d}'] = xings[d]

        def cfg(name, domain):
            return xings[domain].config(
                regs[name], init=table[name].reset, name=name.lower())

        def cmd(name, domain):
            return xings[domain].command(
                regs[name], regs.wstb[name], name=name.lower())

        # ── id block: guard, snapshot control, timestamp ────────
        guard_lock = regs.field('GUARD_LOCK', 'lock')
        m.d.comb += [
            regs.inhibit['GUARD_LO'].eq(guard_lock),
            regs.inhibit['GUARD_HI'].eq(guard_lock),
            regs.inhibit['GUARD_LOCK'].eq(guard_lock),
            regs.clear['GUARD_LOCK'].eq(core_reset),
        ]
        guard = {
            d: (cfg('GUARD_LO', d), cfg('GUARD_HI', d))
            for d in ['sync', 'clk2x']}

        ts = Signal(64)
        m.d.sync += ts.eq(ts + 1)

        # Device DNA (read once after the AXI-Lite reset)
        m.submodules.dna = DomainRenamer({'sync': 's_axi_lite'})(self.dna)
        drive('DNA_LO', self.dna.dna[:32])
        drive('DNA_HI', self.dna.dna[32:])
        drive('DNA_STATUS', self.dna.valid)

        # ── Ingest (sampling) ───────────────────────────────────
        ingest = self.ingest
        ingest_cdc = self.ingest_cdc
        m.submodules.ingest = ingest
        m.submodules.ingest_cdc = ingest_cdc
        ictrl = cfg('INGEST_CTRL', 'sampling')
        icmd = cmd('INGEST_CMD', 'sampling')
        m.d.comb += [
            ingest.re_in.eq(self.re_in),
            ingest.im_in.eq(self.im_in),
            ingest.valid_in.eq(self.valid_in),
            ingest.stats_enable.eq(ictrl[0]),
            ingest.prbs_enable.eq(ictrl[1]),
            ingest.prbs_mode.eq(ictrl[2]),
            ingest.honor_valid.eq(ictrl[3]),
            ingest.clear_on_snap.eq(ictrl[4]),
            ingest.clear.eq(icmd[0]),
            ingest_cdc.re_in.eq(self.re_in),
            ingest_cdc.im_in.eq(self.im_in),
            ingest_cdc.valid_in.eq(self.valid_in),
            ingest_cdc.honor_valid.eq(ictrl[3]),
            ingest_cdc.clear.eq(icmd[0]),
            ingest_cdc.reset.eq(fifo_reset),
        ]

        # Live IQ in sync: 12-bit two's complement -> signed 16 (as p25_top
        # does for the wideband ring with .as_signed()).
        live_re = Signal(signed(16))
        live_im = Signal(signed(16))
        live_strobe = Signal()
        m.d.comb += [
            live_re.eq(ingest_cdc.re_out.as_signed()),
            live_im.eq(ingest_cdc.im_out.as_signed()),
            live_strobe.eq(ingest_cdc.strobe_out),
        ]

        # ── Legacy ring (sync) ──────────────────────────────────
        legacy = self.legacy
        assert (legacy.base, legacy.size, legacy.num_buffers) == (
            table['LEGACY_BASE'].const, table['LEGACY_SIZE'].const,
            table['LEGACY_NUM_BUFFERS'].const)
        lrate = self.legacy_rate
        lpat = self.legacy_pattern
        m.submodules.legacy = legacy
        m.submodules.legacy_rate = lrate
        m.submodules.legacy_pattern = lpat
        lctrl = cfg('LEGACY_CTRL', 'sync')
        lcmd = cmd('LEGACY_CMD', 'sync')
        lsrc = lctrl[1:3]
        m.d.comb += [
            lrate.inc.eq(cfg('LEGACY_RATE_INC', 'sync')),
            lrate.enable.eq(lsrc == SamplePattern.RAMP),
            lpat.mode.eq(lsrc),
            lpat.strobe.eq(lrate.strobe),
            lpat.live_re.eq(live_re),
            lpat.live_im.eq(live_im),
            lpat.live_strobe.eq(live_strobe),
            lpat.clear.eq(lcmd[0]),
            legacy.re_in.eq(lpat.re),
            legacy.im_in.eq(lpat.im),
            legacy.strobe_in.eq(lpat.strobe_out),
            legacy.enable.eq(lctrl[0]),
            legacy.clear.eq(lcmd[0]),
            legacy.hist_sel.eq(cfg('LEGACY_HIST_SEL', 'sync')),
        ]

        # ── Ring v2 (sync) ──────────────────────────────────────
        ring = self.ringv2
        rrate = self.ringv2_rate
        wpat = self.ringv2_pattern
        m.submodules.ringv2 = ring
        m.submodules.ringv2_rate = rrate
        m.submodules.ringv2_pattern = wpat
        rctrl = cfg('RINGV2_CTRL', 'sync')
        rcmd = cmd('RINGV2_CMD', 'sync')
        rsrc = rctrl[3:6]

        # Live IQ packer, same layout as p25_hdl.IQPacker:
        # {im1, re1, im0, re0}, 16 bits each, sample 0 in the low half.
        pk_phase = Signal()
        pk_low = Signal(32)
        pk_word = Signal(64)
        pk_valid = Signal()
        m.d.sync += pk_valid.eq(0)
        with m.If(rcmd[2]):
            m.d.sync += pk_phase.eq(0)
        with m.Elif(live_strobe):
            with m.If(~pk_phase):
                m.d.sync += [
                    pk_low.eq(Cat(live_re, live_im)),
                    pk_phase.eq(1),
                ]
            with m.Else():
                m.d.sync += [
                    pk_word.eq(Cat(pk_low, live_re, live_im)),
                    pk_valid.eq(1),
                    pk_phase.eq(0),
                ]

        irq_every = cfg('RINGV2_IRQ_EVERY', 'sync')
        irq_timeout = cfg('RINGV2_IRQ_TIMEOUT', 'sync')
        m.d.comb += [
            rrate.inc.eq(cfg('RINGV2_RATE_INC', 'sync')),
            rrate.enable.eq((rsrc != WordPattern.OFF)
                            & (rsrc != WordPattern.LIVE)),
            wpat.mode.eq(rsrc),
            wpat.tag.eq(rctrl[12:16]),
            wpat.strobe.eq(rrate.strobe),
            wpat.live_data.eq(pk_word),
            wpat.live_valid.eq(pk_valid),
            wpat.clear.eq(rcmd[2]),
            ring.in_data.eq(wpat.data),
            ring.in_valid.eq(wpat.valid),
            ring.enable.eq(rctrl[0]),
            ring.protect.eq(rctrl[1]),
            ring.header_enable.eq(rctrl[2]),
            ring.base.eq(cfg('RINGV2_BASE', 'sync')),
            ring.size_bursts.eq(cfg('RINGV2_SIZE_BURSTS', 'sync')),
            ring.subbuf_bursts.eq(cfg('RINGV2_SUBBUF_BURSTS', 'sync')),
            ring.irq_every.eq(Mux(rctrl[8], irq_every, 0)),
            ring.irq_timeout.eq(Mux(rctrl[9], irq_timeout, 0)),
            ring.flush_timeout.eq(cfg('RINGV2_FLUSH_TIMEOUT', 'sync')),
            ring.max_outstanding_cfg.eq(
                cfg('RINGV2_MAX_OUTSTANDING', 'sync')),
            # Streaming consumer pointer: its own coherent crossing (it
            # changes continuously while streaming; not ordered with the
            # rest of the configuration).
            ring.consumer_bursts.eq(config_sync(
                m, regs['RINGV2_CONSUMER_BURSTS'], 's_axi_lite', 'sync',
                name='ringv2_consumer_bursts').o),
            ring.guard_lo.eq(guard['sync'][0]),
            ring.guard_hi.eq(guard['sync'][1]),
            ring.soft_reset.eq(rcmd[0]),
            ring.flush.eq(rcmd[1]),
            ring.clear.eq(rcmd[2]),
            ring.hist_sel.eq(cfg('RINGV2_HIST_SEL', 'sync')),
        ]
        m.submodules.committed_sync = committed = GrayCounterSync(
            'sync', 's_axi_lite', 32)
        m.d.comb += committed.i.eq(ring.committed_bursts)
        drive('RINGV2_COMMITTED_BURSTS', committed.o)
        drive('RINGV2_STATUS', ff_sync(
            m, Cat(ring.idle, ring.enabled, ring.fifo_empty), 's_axi_lite',
            name='ringv2_status'))

        # ── Memory testers (clk2x = mem) ────────────────────────
        mem_snap = {}
        for n, mt in [(0, self.mt0), (1, self.mt1)]:
            p = f'MT{n}_'
            m.submodules[f'mt{n}'] = DomainRenamer({'sync': 'clk2x'})(mt)
            ctrl = cfg(p + 'CTRL', 'clk2x')
            mcmd = cmd(p + 'CMD', 'clk2x')
            m.d.comb += [
                mt.mode.eq(ctrl[0:3]),
                mt.pattern.eq(ctrl[3:7]),
                mt.burst_len.eq(ctrl[7:12]),
                mt.max_outstanding_cfg.eq(ctrl[12:16]),
                mt.stop_on_error.eq(ctrl[16]),
                mt.base.eq(cfg(p + 'BASE', 'clk2x')),
                mt.size.eq(cfg(p + 'SIZE', 'clk2x')),
                mt.passes.eq(cfg(p + 'PASSES', 'clk2x')),
                mt.idle_cycles.eq(cfg(p + 'IDLE_CYCLES', 'clk2x')),
                mt.seed.eq(cfg(p + 'SEED', 'clk2x')),
                mt.guard_lo.eq(guard['clk2x'][0]),
                mt.guard_hi.eq(guard['clk2x'][1]),
                mt.hist_sel.eq(cfg(p + 'HIST_SEL', 'clk2x')),
                mt.start.eq(mcmd[0]),
                mt.abort.eq(mcmd[1]),
                mt.clear.eq(mcmd[2]),
            ]
            drive(p + 'STATUS', ff_sync(
                m, Cat(mt.busy, mt.done, mt.error), 's_axi_lite',
                name=f'mt{n}_status'))
            mem_snap.update({
                p + 'PASS_COUNT': mt.pass_count,
                p + 'BYTES_WR': mt.bytes_wr,
                p + 'BYTES_RD': mt.bytes_rd,
                p + 'CYCLES': mt.cycles,
                p + 'ERR_COUNT': mt.err_count,
                p + 'FIRST_ERR_ADDR': mt.first_err_addr,
                p + 'FIRST_ERR_EXP': mt.first_err_exp,
                p + 'FIRST_ERR_ACT': mt.first_err_act,
                p + 'ERR_LANES': mt.err_lanes,
                p + 'BRESP_ERR': mt.bresp_err,
                p + 'RRESP_ERR': mt.rresp_err,
                p + 'WLAT_MAX': mt.wlat_max,
                p + 'RLAT_MAX': mt.rlat_max,
                p + 'HIST_VAL': mt.hist_val,
                p + 'GUARD_BLOCKED': mt.guard_blocked,
            })

        # ── Event recorder (sync -> s_axi_lite) ─────────────────
        evt = self.evt
        m.submodules.evt = evt
        ectrl = cfg('EVT_CTRL', 'sync')
        ecmd = cmd('EVT_CMD', 'sync')
        m.d.comb += [
            evt.ctrl_in.eq(self.ctrl_out),
            evt.enable.eq(ectrl[0]),
            evt.mask.eq(ectrl[8:16]),
            evt.clear.eq(ecmd[0]),
            evt.reset.eq(fifo_reset),
            evt.pop.eq(regs.field('EVT_POP', 'pop')),
        ]
        drive('EVT_LEVEL', evt.level)
        drive('EVT_DATA_LO', evt.data[:32])
        drive('EVT_DATA_HI', evt.data[32:36])
        drive('EVT_CURRENT', ff_sync(m, evt.current, 's_axi_lite',
                                     name='evt_current'))

        # ── Clock census (s_axi_lite) ───────────────────────────
        census = self.census
        m.submodules.census = census
        m.d.comb += [
            census.start.eq(regs.field('CENSUS_CTRL', 'start')),
            census.gate.eq(regs['CENSUS_GATE']),
            census.inputs['clkout'].eq(self.ad_clkout),
        ]
        drive('CENSUS_STATUS', Cat(census.busy, census.done))
        drive('CENSUS_GATE_ACTUAL', census.gate_actual)
        for suffix, name, _, _ in CENSUS_CLOCKS:
            drive(f'CENSUS_{suffix}', census.counts[name])
        drive('CENSUS_CLKOUT', census.counts['clkout'])

        # ── Snapshots ───────────────────────────────────────────
        sync_snap = {
            'TS': ts,
            'LEGACY_LAST_BUFFER': legacy.last_buffer,
            'LEGACY_NEXT_ADDRESS': legacy.next_address,
            'LEGACY_PACKER_OVF': legacy.packer_ovf,
            'LEGACY_PACKER_OVF_DISABLED': legacy.packer_ovf_disabled,
            'LEGACY_WORDS_IN': legacy.words_in,
            'LEGACY_WORDS_ACCEPTED': legacy.words_accepted,
            'LEGACY_AW': legacy.aw_count,
            'LEGACY_B': legacy.b_count,
            'LEGACY_BRESP_ERR': legacy.bresp_err,
            'LEGACY_SUBBUF_DONE': legacy.subbuf_done,
            'LEGACY_STALL_CYCLES': legacy.stall_cycles,
            'LEGACY_MAX_STALL': legacy.max_stall,
            'LEGACY_MAX_OUTSTANDING': legacy.max_outstanding,
            'LEGACY_LAT_MAX': legacy.lat_max,
            'LEGACY_HIST_VAL': legacy.hist_val,
            'LEGACY_GEN': lpat.count,
            'RINGV2_ISSUED_BURSTS': ring.issued_bursts,
            'RINGV2_WORDS_IN': ring.words_in,
            'RINGV2_DROP_FULL': ring.drop_full,
            'RINGV2_DROP_PROTECT': ring.drop_protect,
            'RINGV2_PAD_WORDS': ring.pad_words,
            'RINGV2_FLUSHES': ring.flushes,
            'RINGV2_BRESP_ERR': ring.bresp_err,
            'RINGV2_FIFO_HWM': ring.fifo_hwm,
            'RINGV2_MAX_OUTSTANDING_SEEN': ring.max_outstanding_seen,
            'RINGV2_LAT_MAX': ring.lat_max,
            'RINGV2_HIST_VAL': ring.hist_val,
            'RINGV2_EPOCH': ring.epoch,
            'RINGV2_HEADERS': ring.headers,
            'RINGV2_GEN': wpat.count,
            'RINGV2_GUARD_BLOCKED': ring.guard_blocked,
            'EVT_OVERFLOWS': evt.overflows,
        }
        sampling_snap = {
            'SAMPLES': ingest.samples,
            'VALID_GAP_CYCLES': ingest.valid_gap_cycles,
            'VALID_GAP_RUNS': ingest.valid_gap_runs,
            'CDC_WRERR': ingest_cdc.wrerr_count,
            'CDC_FULL_CYCLES': ingest_cdc.full_cycles,
            'I_MIN': ingest.i_min,
            'I_MAX': ingest.i_max,
            'Q_MIN': ingest.q_min,
            'Q_MAX': ingest.q_max,
            'I_SUM': ingest.i_sum,
            'Q_SUM': ingest.q_sum,
            'I_SUMSQ': ingest.i_sumsq,
            'Q_SUMSQ': ingest.q_sumsq,
            'CLIP_COUNT': ingest.clip_count,
            'I_OR_MASK': ingest.i_or_mask,
            'I_AND_MASK': ingest.i_and_mask,
            'Q_OR_MASK': ingest.q_or_mask,
            'Q_AND_MASK': ingest.q_and_mask,
            'WIN_SAMPLES': ingest.win_samples,
            'PRBS_CHECKED': ingest.prbs_checked,
            'PRBS_ERRORS': ingest.prbs_errors,
            'PRBS_OOS_EVENTS': ingest.prbs_oos_events,
            'PRBS_STATUS': ingest.prbs_in_sync,
        }
        snap_specs = [
            # (snapshot domain, clock domain, signals, latch_delay)
            ('sync', 'sync', sync_snap, 0),
            ('mem', 'clk2x', mem_snap, 0),
            # The ingest window statistics are frozen by `snap` and valid
            # one cycle later (snap_done), so latch 2 cycles after trigger.
            ('sampling', 'sampling', sampling_snap, 2),
        ]
        snaps = {}
        for snap_name, domain, signals, latch_delay in snap_specs:
            snap = Snapshot(
                domain, {k.lower(): v for k, v in signals.items()},
                req_domain='s_axi_lite', latch_delay=latch_delay)
            m.submodules[f'snap_{snap_name}'] = snap
            snaps[snap_name] = snap
            for k in signals:
                shadow = snap.shadow[k.lower()]
                if f'{k}_LO' in table:
                    drive64(k, shadow)
                else:
                    drive(k, shadow)
        m.d.comb += ingest.snap.eq(snaps['sampling'].trigger)

        snap_req = regs['SNAP_REQ']
        snap_mask = Signal(len(snap_req))
        snap_seq = Signal(32)
        with m.If(regs.wstb['SNAP_REQ']):
            axil += [
                snap_mask.eq(snap_req),
                snap_seq.eq(snap_seq + 1),
            ]
        acks = []
        for snap_name, domain, _, _ in snap_specs:
            bit = self.config.snapshot_domains[snap_name].bit_length() - 1
            snap = snaps[snap_name]
            m.d.comb += [
                snap.req.eq(snap_req[bit]),
                # writes made before SNAP_REQ are visible in the snapshot
                snap.hold_off.eq(xings[domain].busy),
            ]
            acks.append((bit, snap.ack))
        ack_vec = Signal(len(snap_req))
        for bit, ack in acks:
            m.d.comb += ack_vec[bit].eq(ack)
        drive('SNAP_ACK', ack_vec & snap_mask)
        drive('SNAP_SEQ', snap_seq)

        # ── Interrupts ──────────────────────────────────────────
        irq_src = Signal(5)
        census_done_q = Signal()
        axil += census_done_q.eq(census.done)
        m.d.comb += [
            irq_src[0].eq(pulse_sync(m, ring.irq, 'sync', 's_axi_lite',
                                     name='irq_ringv2')),
            irq_src[1].eq(pulse_sync(m, legacy.irq, 'sync', 's_axi_lite',
                                     name='irq_legacy')),
            irq_src[2].eq(pulse_sync(m, self.mt0.irq, 'clk2x',
                                     's_axi_lite', name='irq_mt0')),
            irq_src[3].eq(pulse_sync(m, self.mt1.irq, 'clk2x',
                                     's_axi_lite', name='irq_mt1')),
            irq_src[4].eq(census.done & ~census_done_q),
        ]
        irq_pending = Signal(5)
        # a new event wins over a simultaneous clear
        axil += irq_pending.eq(
            (irq_pending & ~regs['IRQ_CLEAR'][:5]) | irq_src)
        pending = Signal(6)
        m.d.comb += pending.eq(Cat(irq_pending, evt.level != 0))
        drive('IRQ_PENDING', pending)
        irq_line = Signal()
        m.d.comb += irq_line.eq((pending & regs['IRQ_ENABLE']).any())
        axil += self.interrupt_out.eq(irq_line)
        irq_count = Signal(32)
        with m.If(irq_line & ~self.interrupt_out & ~irq_count.all()):
            axil += irq_count.eq(irq_count + 1)
        drive('IRQ_COUNT', irq_count)

        # ── Completeness check ──────────────────────────────────
        missing = [r.name for r in table.registers()
                   if r.access == 'ro' and r.const is None
                   and r.name not in driven]
        if missing:
            raise RuntimeError(f'ro registers not driven: {missing}')

        return m


def write_svd(path, config=None):
    with open(path, 'wb') as f:
        f.write(build_register_table(config).to_svd())


def write_json(path, config=None):
    with open(path, 'w', newline='\n') as f:
        f.write(build_register_table(config).to_json())


def write_markdown(path, config=None):
    with open(path, 'w', newline='\n') as f:
        f.write(build_register_table(config).to_markdown(
            title='hwval register map', preamble=MARKDOWN_PREAMBLE))


def parse_args():
    parser = argparse.ArgumentParser(
        description='Generate Fishball hwval IP core Verilog and register '
                    'map files')
    parser.add_argument(
        '--config', default='default',
        help='hwval configuration name [default=%(default)r]')
    parser.add_argument('--svd', help='Output SVD file')
    parser.add_argument('--json', help='Output JSON register map')
    parser.add_argument('--md', help='Output Markdown register map')
    parser.add_argument(
        'output_file', nargs='?', help='Output Verilog file')
    return parser.parse_args()


def main():
    args = parse_args()
    config = config_mod.configs[args.config]()
    if args.output_file:
        top = HwvalCore(config)
        platform = PlutoPlatform()
        with open(args.output_file, 'w') as f:
            f.write(amaranth.back.verilog.convert(
                top, platform=platform, ports=top.ports()))
    if args.svd:
        write_svd(args.svd, config)
    if args.json:
        write_json(args.json, config)
    if args.md:
        write_markdown(args.md, config)


if __name__ == '__main__':
    main()
