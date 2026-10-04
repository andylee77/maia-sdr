#
# Fishball hwval -- RX IQ ingest monitor and valid-honouring IQ CDC
#
# Blocks `ingest` of doc/HW_VALIDATION_SUITE.md section 6.4.
#
# IngestMonitor watches the raw 12-bit AD9361 samples in the `sampling`
# domain (the same `re_in`/`im_in` slices that p25_core feeds to its
# RxIQCDC) and measures what actually arrives: sample and valid-gap
# counts, windowed statistics (min/max/sum/sum of squares, clipping,
# stuck-bit masks) and a PRBS checker ported from the ADI
# `axi_ad9361_rx_pnmon` / `ad_pnmon` Verilog, extended with a real
# error counter.
#
# IngestCDC is the sampling -> sync crossing. Unlike maia_hdl RxIQCDC,
# which writes the FIFO on every sampling clock and ignores `valid`, it
# can honour `valid_in` and it exposes FIFO overflow counters.
#
# This module also hosts two things shared with event_rec.py:
#
#   * AsyncFifo36: the Xilinx FIFO18E1 (maia_hdl AsyncFifo18_36) for
#     synthesis, or a behavioural model built on
#     amaranth.lib.fifo.AsyncFIFO when sim=True (pysim cannot simulate
#     the FIFO18E1 Instance).
#   * sat_inc(): 32-bit saturating counter increment helper.
#
# Pure-Python reference generators `ad9361_bist_prbs()` and
# `pn9_pn11()` define the checked sequences for host software.
#

from amaranth import *
from amaranth.lib.cdc import FFSynchronizer
from amaranth.lib.fifo import AsyncFIFO

from maia_hdl.fifo import AsyncFifo18_36


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------

def sat_inc(m, domain, counter, cond):
    """Increment `counter` in `domain` when `cond`, saturating at all-ones.

    Statements added later in the same domain (e.g. a clear) take
    precedence, following Amaranth's last-assignment-wins rule.
    """
    with m.If(cond & ~counter.all()):
        m.d[domain] += counter.eq(counter + 1)


class _AsyncFifo36Sim(Elaboratable):
    """Simulation model of AsyncFifo18_36 (FIFO18E1, FIFO18_36 mode).

    Same ports and read timing as the primitive in standard (non-FWFT)
    mode: `data_out` updates on the `r_domain` clock edge at which
    `rden & ~empty` is sampled. `wrerr`/`rderr` are registered one cycle
    after the offending access, like the primitive. Depth is 512.

    The asynchronous `reset` input is ignored by this model (the
    AsyncFIFO is reset by the write domain reset instead).
    """
    def __init__(self, r_domain, w_domain):
        self._r_domain = r_domain
        self._w_domain = w_domain
        self.reset = Signal()

        self.data_in = Signal(36)
        self.wren = Signal()
        self.full = Signal()
        self.wrerr = Signal()

        self.data_out = Signal(36)
        self.rden = Signal()
        self.empty = Signal()
        self.rderr = Signal()

    def elaborate(self, platform):
        m = Module()
        m.submodules.fifo = fifo = AsyncFIFO(
            width=36, depth=512, r_domain=self._r_domain,
            w_domain=self._w_domain)
        m.d.comb += [
            fifo.w_data.eq(self.data_in),
            fifo.w_en.eq(self.wren),
            self.full.eq(~fifo.w_rdy),
            fifo.r_en.eq(self.rden),
            self.empty.eq(~fifo.r_rdy),
        ]
        m.d[self._w_domain] += self.wrerr.eq(self.wren & ~fifo.w_rdy)
        m.d[self._r_domain] += self.rderr.eq(self.rden & ~fifo.r_rdy)
        with m.If(self.rden & fifo.r_rdy):
            m.d[self._r_domain] += self.data_out.eq(fifo.r_data)
        return m


class AsyncFifo36(Elaboratable):
    """36-bit x 512 asynchronous FIFO (FIFO18E1 or simulation model).

    Thin selector: with ``sim=False`` it instantiates
    maia_hdl.fifo.AsyncFifo18_36 (Xilinx FIFO18E1 in FIFO18_36 mode,
    standard read mode); with ``sim=True`` it instantiates an
    amaranth.lib.fifo.AsyncFIFO based model with the same ports and read
    timing, which pysim can simulate.

    Parameters
    ----------
    r_domain : str
        Read clock domain.
    w_domain : str
        Write clock domain.
    sim : bool
        Use the simulation model instead of the FIFO18E1 Instance.

    Attributes
    ----------
    reset : Signal(), in
        Asynchronous FIFO reset (FIFO18E1 RST; needs >= 5 cycles of the
        slower clock, and WREN/RDEN low around it). Ignored when sim=True.
    data_in : Signal(36), in (w_domain)
    wren : Signal(), in (w_domain)
    full : Signal(), out (w_domain)
    wrerr : Signal(), out (w_domain)
    data_out : Signal(36), out (r_domain)
        Valid on the cycle after ``rden & ~empty``.
    rden : Signal(), in (r_domain)
    empty : Signal(), out (r_domain)
    rderr : Signal(), out (r_domain)
    """
    depth = 512

    def __init__(self, r_domain='read', w_domain='write', sim=False):
        self._r_domain = r_domain
        self._w_domain = w_domain
        self._sim = sim
        self.reset = Signal()

        self.data_in = Signal(36)
        self.wren = Signal()
        self.full = Signal()
        self.wrerr = Signal()

        self.data_out = Signal(36)
        self.rden = Signal()
        self.empty = Signal()
        self.rderr = Signal()

    def elaborate(self, platform):
        m = Module()
        if self._sim:
            fifo = _AsyncFifo36Sim(r_domain=self._r_domain,
                                   w_domain=self._w_domain)
        else:
            fifo = AsyncFifo18_36(r_domain=self._r_domain,
                                  w_domain=self._w_domain)
        m.submodules.fifo = fifo
        m.d.comb += [
            fifo.reset.eq(self.reset),
            fifo.data_in.eq(self.data_in),
            fifo.wren.eq(self.wren),
            self.full.eq(fifo.full),
            self.wrerr.eq(fifo.wrerr),
            self.data_out.eq(fifo.data_out),
            fifo.rden.eq(self.rden),
            self.empty.eq(fifo.empty),
            self.rderr.eq(fifo.rderr),
        ]
        return m


# ---------------------------------------------------------------------------
# PRBS definitions (shared by the HDL checker and the Python references)
# ---------------------------------------------------------------------------

# ADI axi_ad9361_rx_pnmon pn0fn (AD9361 BIST PRBS), 16-bit state:
#   dout = {din[14:0], ((^din[15:4]) ^ (^din[2:1]))};
PN0_WIDTH = 16
# ITU-T O.150 PN9 (x^9 + x^5 + 1) and PN11 (x^11 + x^9 + 1), as the
# PRBS_P09 / PRBS_P11 cases of ADI pn1fn: s[m] = s[m-9] ^ s[m-5] and
# s[m] = s[m-11] ^ s[m-9], serialized MSB first.
PN9_TAPS = (9, 5)
PN11_TAPS = (11, 9)
# ad_pnmon OOS_THRESHOLD: 16 consecutive matches to gain sync, 16
# consecutive mismatches to lose it.
OOS_THRESHOLD = 16


def pn0fn(x):
    """ADI pn0fn: next 16-bit AD9361 BIST PRBS word."""
    fb = (bin((x >> 4) & 0xfff).count('1')
          + bin((x >> 1) & 0x3).count('1')) & 1
    return ((x << 1) & 0xffff) | fb


def bitrev12(x):
    """Reverse the 12 bits of x (ADI brfn)."""
    return int(f'{x & 0xfff:012b}'[::-1], 2)


def pn_step_masks(width, taps):
    """XOR masks of a `width`-bit PN word step for s[m] = XOR s[m - t].

    The word is serialized MSB first (bit width-1 is the oldest bit,
    bit 0 the newest), as in ADI pn1fn. Returns a list `masks` such that
    bit j of the next word is the XOR of the bits of the current word
    selected by ``masks[j]``. Requires max(taps) <= width.

    For width=24 this reproduces the ADI pn1fn PRBS_P09/PRBS_P11 tables
    exactly (see test_hwval_ingest.py).
    """
    if max(taps) > width:
        raise ValueError('tap longer than word width')
    # hist[t] is the mask for bit s[n + t]; the current word holds
    # s[n - k] in bit k.
    hist = {-k: 1 << k for k in range(width)}
    masks = [0] * width
    for j in range(1, width + 1):
        mask = 0
        for t in taps:
            mask ^= hist[j - t]
        hist[j] = mask
        masks[width - j] = mask
    return masks


def pn_step(x, masks):
    """Apply the step described by `masks` (see pn_step_masks) to x."""
    y = 0
    for j, mask in enumerate(masks):
        y |= (bin(x & mask).count('1') & 1) << j
    return y


_PN9_12 = pn_step_masks(12, PN9_TAPS)
_PN11_12 = pn_step_masks(12, PN11_TAPS)
_PN9_24 = pn_step_masks(24, PN9_TAPS)
_PN11_24 = pn_step_masks(24, PN11_TAPS)


def ad9361_bist_prbs(n, seed=0xffff):
    """AD9361 BIST PRBS as seen at the FPGA (ADI pn_sel 0 model).

    Model of axi_ad9361_rx_pnmon (pn0 path): a 16-bit word S advanced
    by pn0fn() once per sample, with

        I = S[15:4]
        Q = bitrev12(S[11:0])

    i.e. the checker's ``{I, bitrev(Q)[3:0]}`` is S and its
    ``I[7:0] == bitrev(Q)[11:4]`` consistency relation holds. `seed` is
    any nonzero 16-bit word (the sequence is maximal length, period
    65535; the hardware phase is arbitrary and the checker
    self-synchronizes).

    Yields `n` tuples (i, q) of 12-bit unsigned bit patterns (two's
    complement values in unsigned containers, like re_in/im_in).
    """
    s = seed & 0xffff
    if s == 0:
        raise ValueError('seed must be nonzero')
    for _ in range(n):
        yield (s >> 4, bitrev12(s))
        s = pn0fn(s)


def pn9_pn11(n):
    """PN9 on I and PN11 on Q, 12 bits per sample (ADI pn_sel 9 model).

    Bit-exact copy of the axi_ad9361_tx_channel DAC PN generator
    (dac_data_sel = 9): a 24-bit register seeded with 0xffffff emits
    [23:12] then [11:0] and is then advanced with pn1fn (PRBS_P09 for
    channel 0 = I, PRBS_P11 for channel 1 = Q). The checker only relies
    on the PN recurrence, so any phase of these streams locks.

    Yields `n` tuples (i, q) of 12-bit unsigned bit patterns.
    """
    si = 0xffffff
    sq = 0xffffff
    count = 0
    while True:
        for shift in (12, 0):
            if count == n:
                return
            yield ((si >> shift) & 0xfff, (sq >> shift) & 0xfff)
            count += 1
        si = pn_step(si, _PN9_24)
        sq = pn_step(sq, _PN11_24)


def _hdl_pn_step(value, masks):
    """Amaranth expression for pn_step(value, masks)."""
    bits = []
    for mask in masks:
        terms = [value[k] for k in range(len(masks)) if (mask >> k) & 1]
        acc = terms[0]
        for t in terms[1:]:
            acc = acc ^ t
        bits.append(acc)
    return Cat(*bits)


def _hdl_pn0fn(value):
    """Amaranth expression for pn0fn (value is 16 bits)."""
    fb = value[4:16].xor() ^ value[1:3].xor()
    return Cat(fb, value[:15])


def _hdl_bitrev(value):
    return Cat(*[value[len(value) - 1 - k] for k in range(len(value))])


# ---------------------------------------------------------------------------
# IngestMonitor
# ---------------------------------------------------------------------------

class IngestMonitor(Elaboratable):
    """RX IQ ingest monitor: counters, window statistics, PRBS checker.

    Clock domain: everything is in the ``sampling`` domain (the
    AD9361 divided data clock, util_ad9361_divclk/clk_out; up to
    61.44 MHz in 1R1T). The configuration inputs are quasi-static
    (change them only while the corresponding enable is 0); `clear` and
    `snap` are single-cycle pulses in the sampling domain.

    Sample definition: a cycle is a *sample* when ``valid_in`` is high,
    or on every cycle when ``honor_valid`` is 0 (which is what p25_core's
    RxIQCDC does).

    Pipeline: inputs are registered (stage A), then products, clip flags
    and PRBS words are registered (stage B), then accumulated (stage C).
    A sample appears in the counters 3 cycles after it is presented.

    Output update classes:

    * Running counters (continuous, update every cycle; cleared only by
      `clear`): `samples`, `valid_gap_cycles`, `valid_gap_runs`,
      `prbs_checked`, `prbs_errors`, `prbs_oos_events`, `prbs_in_sync`.
    * Window statistics (frozen registers, updated only by `snap`,
      cleared by `clear`): `i_min`, `i_max`, `q_min`, `q_max`, `i_sum`,
      `q_sum`, `i_sumsq`, `q_sumsq`, `clip_count`, the four masks and
      `win_samples`. On `snap` the outputs load the window accumulated
      so far (every sample that reached stage C before the snap cycle);
      with `clear_on_snap` the next window starts with the sample (if
      any) accumulated in the snap cycle, so no sample is lost or
      counted twice. The frozen values are valid from the cycle in which
      `snap_done` is high (one cycle after `snap`) onwards.

    An empty window reads min = +2047, max = -2048 (min > max), sums 0,
    OR masks 0 and AND masks 0xfff. 32-bit counters saturate at
    2**32 - 1. `i_sum`/`q_sum` are exact for windows up to 2**36 samples
    (~18 min at 61.44 MSPS of full-scale DC); snap more often than that.

    PRBS checker (port of ADI axi_ad9361_rx_pnmon + ad_pnmon):

    * prbs_mode 0 (AD9361 BIST PRBS, ADI pn_sel 0): per sample the word
      ``S = {I[11:0], bitrev(Q)[3:0]}`` is checked against pn0fn, and
      ``I[7:0] == bitrev(Q)[11:4]`` must hold.
    * prbs_mode 1 (ADI pn_sel 9 in 1R1T): I carries PN9 and Q carries
      PN11, 12 bits per sample MSB first. Each 12-bit sample is predicted
      from the previous one (ADI predicts 24-bit pairs from 24-bit pairs;
      the accepted streams are identical) and I and Q must both match.
    * Out of sync, the prediction is seeded from the received sample
      (self-synchronizing); in sync it free-runs, so one corrupted sample
      costs exactly one error. An all-zero word/sample never matches.
      Sync is gained after 16 consecutive matches and lost after 16
      consecutive mismatches (OOS_THRESHOLD = 16, 4-bit counter, same
      toggle rule as ad_pnmon).
    * Differences from ADI: the checker starts out of sync (ADI starts
      "in sync" and needs 16 mismatches first); `prbs_errors` counts
      every mismatch while in sync (ADI has a sticky bit), so dropping a
      sample in sync costs exactly 16 errors and one OOS event; an I/Q
      consistency failure in mode 0 is a forced mismatch (ADI substitutes
      0xdead, which matches if the prediction happens to be 0xdead).
    * `prbs_checked` counts samples compared while in sync (the BER
      denominator; includes the mismatches that end a sync period).
      Disabling `prbs_enable` drops sync; `clear` clears the counters but
      keeps the sync state.

    Data path assumptions for mode 0: the AD9361 raw 12-bit words must
    reach re_in/im_in unmodified (axi_ad9361 DC filter and IQ
    correction off, data format two's complement, which are the
    defaults), as the ADI pnmon sits before those blocks.

    Attributes
    ----------
    re_in : Signal(12), in
        I sample (two's complement bits in an unsigned container).
    im_in : Signal(12), in
        Q sample (two's complement bits in an unsigned container).
    valid_in : Signal(), in
        Sample valid (util_ad9361_adc_fifo dout_valid_0).
    stats_enable : Signal(), in
        Enables sample/gap counters and window statistics.
    prbs_enable : Signal(), in
        Enables the PRBS checker (0 forces out of sync).
    prbs_mode : Signal(), in
        0: AD9361 BIST PRBS (pn0fn). 1: PN9 (I) / PN11 (Q).
    honor_valid : Signal(), in
        1: only valid_in cycles are samples. 0: every cycle is a sample.
    clear : Signal(), in
        Pulse: clear all counters, accumulators and frozen outputs.
    snap : Signal(), in
        Pulse: freeze the window statistics into the outputs.
    clear_on_snap : Signal(), in
        Start a new window on every snap.
    snap_done : Signal(), out
        High for one cycle when the frozen outputs have been updated
        (the cycle after `snap`). Capture snapshots on or after it.
    samples : Signal(64), out
        Running count of samples (stats_enable).
    valid_gap_cycles : Signal(32), out
        Running count of cycles with valid_in = 0 (stats_enable).
    valid_gap_runs : Signal(32), out
        Running count of runs of valid_in = 0 (stats_enable).
    i_min, i_max, q_min, q_max : Signal(signed(12)), out
        Window extrema.
    i_sum, q_sum : Signal(signed(48)), out
        Window sums.
    i_sumsq, q_sumsq : Signal(64), out
        Window sums of squares.
    clip_count : Signal(32), out
        Window count of samples where I or Q is clipped (|x| >= 2047,
        which includes x == -2048).
    i_or_mask, i_and_mask, q_or_mask, q_and_mask : Signal(12), out
        Window OR / AND of the sample bits (stuck-at-0 bits read 0 in
        the OR mask, stuck-at-1 bits read 1 in the AND mask).
    win_samples : Signal(64), out
        Number of samples in the frozen window.
    prbs_checked : Signal(64), out
        Running count of samples checked while in sync.
    prbs_errors : Signal(32), out
        Running count of mismatching samples while in sync.
    prbs_oos_events : Signal(32), out
        Running count of in-sync -> out-of-sync transitions.
    prbs_in_sync : Signal(), out
        PRBS checker in sync.
    """
    def __init__(self, domain='sampling'):
        self._domain = domain
        self.w = 12

        self.re_in = Signal(self.w)
        self.im_in = Signal(self.w)
        self.valid_in = Signal()

        self.stats_enable = Signal()
        self.prbs_enable = Signal()
        self.prbs_mode = Signal()
        self.honor_valid = Signal()
        self.clear = Signal()
        self.snap = Signal()
        self.clear_on_snap = Signal()
        self.snap_done = Signal()

        self.samples = Signal(64)
        self.valid_gap_cycles = Signal(32)
        self.valid_gap_runs = Signal(32)
        self.i_min = Signal(signed(self.w), init=2**(self.w - 1) - 1)
        self.i_max = Signal(signed(self.w), init=-2**(self.w - 1))
        self.q_min = Signal(signed(self.w), init=2**(self.w - 1) - 1)
        self.q_max = Signal(signed(self.w), init=-2**(self.w - 1))
        self.i_sum = Signal(signed(48))
        self.q_sum = Signal(signed(48))
        self.i_sumsq = Signal(64)
        self.q_sumsq = Signal(64)
        self.clip_count = Signal(32)
        self.i_or_mask = Signal(self.w)
        self.i_and_mask = Signal(self.w, init=2**self.w - 1)
        self.q_or_mask = Signal(self.w)
        self.q_and_mask = Signal(self.w, init=2**self.w - 1)
        self.win_samples = Signal(64)
        self.prbs_checked = Signal(64)
        self.prbs_errors = Signal(32)
        self.prbs_oos_events = Signal(32)
        self.prbs_in_sync = Signal()

    def elaborate(self, platform):
        m = Module()
        d = self._domain
        w = self.w
        smax = 2**(w - 1) - 1
        smin = -2**(w - 1)
        ones = 2**w - 1

        # ── Stage A: register inputs ─────────────────────────────────
        re_a = Signal(signed(w))
        im_a = Signal(signed(w))
        vin_a = Signal()
        samp_a = Signal()
        m.d[d] += [
            re_a.eq(self.re_in.as_signed()),
            im_a.eq(self.im_in.as_signed()),
            vin_a.eq(self.valid_in),
            samp_a.eq(self.valid_in | ~self.honor_valid),
        ]

        # ── Stage B: products, clip flags, PRBS words ───────────────
        re_b = Signal(signed(w))
        im_b = Signal(signed(w))
        vin_b = Signal()
        samp_b = Signal()
        i2_b = Signal(2 * w - 1)
        q2_b = Signal(2 * w - 1)
        clip_b = Signal()

        def clipped(x):
            return (x >= smax) | (x <= -smax)

        m.d[d] += [
            re_b.eq(re_a),
            im_b.eq(im_a),
            vin_b.eq(vin_a),
            samp_b.eq(samp_a),
            i2_b.eq(re_a * re_a),
            q2_b.eq(im_a * im_a),
            clip_b.eq(clipped(re_a) | clipped(im_a)),
        ]

        # PRBS word: bits [11:0] = Q-part, [23:12] = I-part.
        #  mode 0: [15:0] = S = {I, bitrev(Q)[3:0]}, [23:16] = 0
        #  mode 1: [11:0] = Q (PN11), [23:12] = I (PN9)
        i_u = re_a.as_unsigned()
        q_u = im_a.as_unsigned()
        q_rev = _hdl_bitrev(q_u)
        s0 = Cat(q_rev[:4], i_u)
        word_b = Signal(24)
        ok_b = Signal()
        with m.If(self.prbs_mode):
            m.d[d] += [
                word_b.eq(Cat(q_u, i_u)),
                ok_b.eq((i_u != 0) & (q_u != 0)),
            ]
        with m.Else():
            m.d[d] += [
                word_b.eq(s0),
                ok_b.eq((i_u[:8] == q_rev[4:]) & (s0 != 0)),
            ]

        # ── Stage C: accumulate ───────────────────────────────────────
        v = Signal()
        m.d.comb += v.eq(samp_b & self.stats_enable)
        snapclr = Signal()
        m.d.comb += snapclr.eq(self.snap & self.clear_on_snap)

        acc_i_min = Signal(signed(w), init=smax)
        acc_i_max = Signal(signed(w), init=smin)
        acc_q_min = Signal(signed(w), init=smax)
        acc_q_max = Signal(signed(w), init=smin)
        acc_i_sum = Signal(signed(48))
        acc_q_sum = Signal(signed(48))
        acc_i_sumsq = Signal(64)
        acc_q_sumsq = Signal(64)
        acc_clip = Signal(32)
        acc_i_or = Signal(w)
        acc_i_and = Signal(w, init=ones)
        acc_q_or = Signal(w)
        acc_q_and = Signal(w, init=ones)
        acc_samples = Signal(64)

        def base(acc, init):
            return Mux(snapclr, C(init, acc.shape()), acc)

        for acc, x in [(acc_i_min, re_b), (acc_q_min, im_b)]:
            b = base(acc, smax)
            m.d[d] += acc.eq(Mux(v & (x < b), x, b))
        for acc, x in [(acc_i_max, re_b), (acc_q_max, im_b)]:
            b = base(acc, smin)
            m.d[d] += acc.eq(Mux(v & (x > b), x, b))
        for acc, x in [(acc_i_sum, re_b), (acc_q_sum, im_b)]:
            m.d[d] += acc.eq(base(acc, 0) + Mux(v, x, 0))
        for acc, x in [(acc_i_sumsq, i2_b), (acc_q_sumsq, q2_b)]:
            m.d[d] += acc.eq(base(acc, 0) + Mux(v, x, 0))
        for acc, x in [(acc_i_or, re_b), (acc_q_or, im_b)]:
            m.d[d] += acc.eq(base(acc, 0) | Mux(v, x.as_unsigned(), 0))
        for acc, x in [(acc_i_and, re_b), (acc_q_and, im_b)]:
            m.d[d] += acc.eq(
                base(acc, ones) & Mux(v, x.as_unsigned(), ones))
        clip_base = base(acc_clip, 0)
        m.d[d] += acc_clip.eq(
            Mux(clip_base.all(), clip_base, clip_base + (v & clip_b)))
        m.d[d] += acc_samples.eq(base(acc_samples, 0) + v)

        window = [
            (self.i_min, acc_i_min), (self.i_max, acc_i_max),
            (self.q_min, acc_q_min), (self.q_max, acc_q_max),
            (self.i_sum, acc_i_sum), (self.q_sum, acc_q_sum),
            (self.i_sumsq, acc_i_sumsq), (self.q_sumsq, acc_q_sumsq),
            (self.clip_count, acc_clip),
            (self.i_or_mask, acc_i_or), (self.i_and_mask, acc_i_and),
            (self.q_or_mask, acc_q_or), (self.q_and_mask, acc_q_and),
            (self.win_samples, acc_samples),
        ]
        with m.If(self.snap):
            m.d[d] += [out.eq(acc) for out, acc in window]
        m.d[d] += self.snap_done.eq(self.snap & ~self.clear)

        # Running counters
        vin_b_q = Signal(init=1)
        m.d[d] += vin_b_q.eq(vin_b)
        with m.If(v):
            m.d[d] += self.samples.eq(self.samples + 1)
        sat_inc(m, d, self.valid_gap_cycles, self.stats_enable & ~vin_b)
        sat_inc(m, d, self.valid_gap_runs,
                self.stats_enable & ~vin_b & vin_b_q)

        # ── Stage C: PRBS checker ─────────────────────────────────────
        pv = Signal()
        m.d.comb += pv.eq(samp_b & self.prbs_enable)
        pred = Signal(24)
        oos = Signal(init=1)
        oos_count = Signal(range(OOS_THRESHOLD))
        match = Signal()
        m.d.comb += match.eq(ok_b & (word_b == pred))
        src = Mux(oos, word_b, pred)
        next_pred0 = Cat(_hdl_pn0fn(src[:16]), C(0, 8))
        next_pred1 = Cat(_hdl_pn_step(src[:12], _PN11_12),
                         _hdl_pn_step(src[12:], _PN9_12))
        update = Signal()
        m.d.comb += update.eq(~(oos ^ match))
        lose_sync = Signal()
        m.d.comb += lose_sync.eq(
            pv & ~oos & update & (oos_count == OOS_THRESHOLD - 1))
        with m.If(pv):
            m.d[d] += pred.eq(Mux(self.prbs_mode, next_pred1, next_pred0))
            with m.If(update):
                with m.If(oos_count == OOS_THRESHOLD - 1):
                    m.d[d] += oos.eq(~oos)
                m.d[d] += oos_count.eq(oos_count + 1)
            with m.Else():
                m.d[d] += oos_count.eq(0)
            with m.If(~oos):
                m.d[d] += self.prbs_checked.eq(self.prbs_checked + 1)
        sat_inc(m, d, self.prbs_errors, pv & ~oos & ~match)
        sat_inc(m, d, self.prbs_oos_events, lose_sync)
        with m.If(~self.prbs_enable):
            m.d[d] += [
                oos.eq(1),
                oos_count.eq(0),
                pred.eq(0),
            ]
        m.d.comb += self.prbs_in_sync.eq(~oos)

        # ── Clear (takes precedence) ──────────────────────────────────
        with m.If(self.clear):
            m.d[d] += [
                acc.eq(acc.init) for acc in [
                    acc_i_min, acc_i_max, acc_q_min, acc_q_max,
                    acc_i_sum, acc_q_sum, acc_i_sumsq, acc_q_sumsq,
                    acc_clip, acc_i_or, acc_i_and, acc_q_or, acc_q_and,
                    acc_samples]
            ]
            m.d[d] += [out.eq(out.init) for out, _ in window]
            m.d[d] += [
                c.eq(0) for c in [
                    self.samples, self.valid_gap_cycles,
                    self.valid_gap_runs, self.prbs_checked,
                    self.prbs_errors, self.prbs_oos_events]
            ]
            # A gap in progress at clear time counts as a new run.
            m.d[d] += vin_b_q.eq(1)

        return m


# ---------------------------------------------------------------------------
# IngestCDC
# ---------------------------------------------------------------------------

class IngestCDC(Elaboratable):
    """sampling -> sync IQ CDC that honours valid and counts overflows.

    Same structure as maia_hdl.cdc.RxIQCDC (FIFO18E1 in FIFO18_36 mode,
    read whenever not empty, `strobe_out` marks each output sample), with
    two differences: when `honor_valid` is 1 only `valid_in` cycles are
    written (RxIQCDC writes every i_domain cycle), and write attempts
    while the FIFO is full are counted instead of silently lost.

    Clock domains: `re_in`, `im_in`, `valid_in`, `clear`, `wrerr_count`
    and `full_cycles` are in `i_domain`; `re_out`, `im_out`,
    `strobe_out` are in `o_domain`. `honor_valid` is quasi-static
    (change it only while the stream is idle or the FIFO is in reset).
    `reset` is asynchronous (same semantics as RxIQCDC.reset): it resets
    the FIFO18E1 and blocks writes through an i_domain synchronizer. The
    integrator must drive it (e.g. from CORE_RESET) and pulse it once
    the sampling clock is running, as p25_core does with sdr_reset.
    XDC: ``set_false_path -to [get_pins <path>/fifo/fifo/fifo18e1/RST]``.

    With `sim=True` the FIFO is the AsyncFIFO model (reset ignored).

    Parameters
    ----------
    i_domain : str
        Input (write) clock domain.
    o_domain : str
        Output (read) clock domain.
    width : int
        Sample width (<= 18).
    sim : bool
        Use the simulation FIFO model.

    Attributes
    ----------
    re_in, im_in : Signal(width), in (i_domain)
    valid_in : Signal(), in (i_domain)
    honor_valid : Signal(), in (quasi-static)
        1: write only valid_in cycles. 0: write every cycle (RxIQCDC).
    clear : Signal(), in (i_domain)
        Pulse: clear wrerr_count and full_cycles.
    wrerr_count : Signal(32), out (i_domain)
        Write attempts while the FIFO was full (samples lost),
        saturating.
    full_cycles : Signal(32), out (i_domain)
        i_domain cycles with the FIFO full, saturating.
    reset : Signal(), in (asynchronous)
        FIFO reset.
    re_out, im_out : Signal(width), out (o_domain)
    strobe_out : Signal(), out (o_domain)
        A new sample is presented on re_out/im_out.
    """
    def __init__(self, i_domain='sampling', o_domain='sync', width=12,
                 sim=False):
        self._i_domain = i_domain
        self._o_domain = o_domain
        self._sim = sim
        self.w = width
        if self.w > 18:
            raise ValueError('width > 18 not supported')

        # i_domain
        self.re_in = Signal(width)
        self.im_in = Signal(width)
        self.valid_in = Signal()
        self.honor_valid = Signal()
        self.clear = Signal()
        self.wrerr_count = Signal(32)
        self.full_cycles = Signal(32)

        # asynchronous
        self.reset = Signal()

        # o_domain
        self.re_out = Signal(width)
        self.im_out = Signal(width)
        self.strobe_out = Signal()

    def elaborate(self, platform):
        m = Module()
        i = self._i_domain
        o = self._o_domain
        m.submodules.fifo = fifo = AsyncFifo36(
            r_domain=o, w_domain=i, sim=self._sim)

        # i_domain
        reset_i = Signal()
        m.submodules.sync_reset = FFSynchronizer(
            self.reset, reset_i, o_domain=i, init=1)
        write = Signal()
        m.d.comb += [
            write.eq(~reset_i & (self.valid_in | ~self.honor_valid)),
            fifo.data_in.eq(Cat(self.re_in, self.im_in)),
            fifo.wren.eq(write),
        ]
        sat_inc(m, i, self.wrerr_count, write & fifo.full)
        sat_inc(m, i, self.full_cycles, ~reset_i & fifo.full)
        with m.If(self.clear):
            m.d[i] += [
                self.wrerr_count.eq(0),
                self.full_cycles.eq(0),
            ]

        # o_domain
        m.d.comb += [
            self.re_out.eq(fifo.data_out[:self.w]),
            self.im_out.eq(fifo.data_out[self.w:2 * self.w]),
            fifo.rden.eq(~fifo.empty),
            fifo.reset.eq(self.reset),
        ]
        m.d[o] += self.strobe_out.eq(fifo.rden)

        return m
