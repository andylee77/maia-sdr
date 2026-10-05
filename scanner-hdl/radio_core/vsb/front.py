#
# Fishball radio core - the 8-VSB front end (pl/front.rs `run`): the AD9361's samples to y, the
# equalizer's input, one a symbol, with the timing loop and the segment and field syncs.
#
#   samples -> matched filter (matched_filter.py) -> datapath and loops (datapath.py) -> y
#                 ^ instants                                     |
#                 +-- scheduler <-- timing error, search result -+-- sync logic
#
# The scheduler holds each symbol's instant (64.32 input samples) and the timing loop's rate, and
# issues the matched filter's evaluations: at a segment sync symbol early, late, then the symbol
# itself, so that all three pass the datapath with the symbol's state; otherwise the symbol alone.
# It waits for the evaluation's last input sample, and after the fourth sync symbol for the timing
# error (late less early), which moves the next instant and the rate:
#
#   - the instant steps by the nominal step (the PS's, from the true sample rate) plus the rate's
#     sum shifted down 16;
#   - at the fourth sync symbol it moves by the error shifted up 14, and the rate's sum by the error
#     shifted up 13, for the first 512 segments; then 10 and 5.
#
# The sync logic follows the model's phases:
#
#   - settle: 2^16 symbols for the loops;
#   - search: the segment sync's correlation at each of a segment's 832 places, summed over 64
#     segments in block RAM; the best place (the last of equal sums) gives the segment's start;
#   - count: each segment's sync, early, late, PN511 and middle PN63 correlations. Over the first
#     313 segments the best PN511 above a quarter of a perfect one is a field sync; then every 313th
#     segment is one, found or not, and four missed in a row start that search again.
#
# Each y goes out with its place: the phase, its place in the segment, and once fields are counted
# its segment in the field and the field's number (the first field sync counted is field 0).
#
# The evaluations come at least 16 cycles apart, so the scheduler and the sync logic take a few
# registered steps each where one cycle of clk3x would not hold the arithmetic.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory

from .datapath import Datapath, KIND_EARLY, KIND_LATE, KIND_MAIN, Y_WIDTH
from .matched_filter import HALF, MatchedFilter
from .tables import FIELD_SEGMENTS, MIDDLE_PN63, SEGMENT, SEGMENT_SYNC, pn511, pn63

SETTLE = 1 << 16
SEARCH_SEGMENTS = 64
# The timing loop (pl/mod.rs `Loops::DEFAULT`): instant and rate shifts, acquiring then tracking.
ACQUIRE_SEGMENTS = 512
TIMING_KP = (14, 10)
TIMING_RATE_SHIFT = (16 - 3, 16 - 11)
RATE_FRAC = 16
RATE_SUM_WIDTH = 48
# A field sync: a quarter of a perfect PN511 correlation (511 symbols of 5 levels, 128 a level).
FIELD_SYNC_THRESHOLD = 511 * 5 * 128 // 4
FIELD_MISSES = 4
# Inputs the buffer keeps behind the newest, less the samples written during an evaluation.
SAFE_LAG = 64 - (HALF - 1) - 2

PHASE_SETTLE = 0
PHASE_SEARCH = 1
PHASE_COUNT = 2

TAG_WIDTH = 2 + 2 + 10 + 32


class Front(Elaboratable):
    """The front end.

    Attributes
    ----------
    wr_en, wr_re, wr_im : in
        A new AD9361 sample.
    step0 : Signal(32), in
        The nominal symbol step, input samples x 2^32.
    carrier0 : Signal(signed(32)), in
        The carrier loop's starting frequency.
    tw_en, tw_addr, tw_data : in
        A matched-filter table word.
    y_valid : out
        One cycle per symbol: ``y`` and its place (``y_phase``, ``y_place``, ``y_fields``,
        ``y_segment``, ``y_field``), its index ``y_n`` and instant ``y_pos`` (64.32), and its
        stages (``t_*``) for the tests.
    side_valid : out
        One cycle per early or late evaluation, for the tests.
    seg_valid : out
        One cycle at each segment counted: its record (``seg_*``).
    field_valid : out
        One cycle at each field sync segment counted: its record (``field_*``).
    field_lost : out
        One cycle: four field syncs missed; the fields are searched again.
    overrun : out
        Sticky: the matched filter fell more than the buffer behind its input.
    """
    def __init__(self, settle=SETTLE, search_segments=SEARCH_SEGMENTS):
        self.settle = settle
        self.search_segments = search_segments

        self.wr_en = Signal()
        self.wr_re = Signal(signed(12))
        self.wr_im = Signal(signed(12))
        self.step0 = Signal(32)
        self.carrier0 = Signal(signed(32))
        self.tw_en = Signal()
        self.tw_addr = Signal(9)
        self.tw_data = Signal(60)

        self.y_valid = Signal()
        self.y = Signal(signed(Y_WIDTH))
        self.y_phase = Signal(2)
        self.y_place = Signal(10)
        self.y_fields = Signal()
        self.y_segment = Signal(9)
        self.y_field = Signal(32)
        self.y_n = Signal(32)
        self.y_pos = Signal(96)
        self.t_filtered = [Signal(signed(13), name=f't_filtered{i}') for i in range(2)]
        self.t_gain = Signal(12)
        self.t_gained = [Signal(signed(12), name=f't_gained{i}') for i in range(2)]
        self.t_theta = Signal(32)
        self.t_turned = [Signal(signed(12), name=f't_turned{i}') for i in range(2)]
        self.t_dc = Signal(signed(16))

        self.side_valid = Signal()
        self.side_late = Signal()
        self.side_n = Signal(32)
        self.side_filtered = [Signal(signed(13), name=f'side_filtered{i}') for i in range(2)]
        self.side_theta = Signal(32)
        self.side_value = Signal(signed(Y_WIDTH))

        self.seg_valid = Signal()
        self.seg_start = Signal(32)
        self.seg_pos = Signal(96)
        self.seg_freq = Signal(signed(35))
        self.seg_rate = Signal(signed(32))
        self.seg_gain = Signal(12)
        self.seg_dc = Signal(signed(16))
        self.seg_sync = Signal(signed(16))
        self.seg_timing = Signal(signed(17))
        self.seg_pn511 = Signal(signed(22))

        self.field_valid = Signal()
        self.field_no = Signal(32)
        self.field_start = Signal(32)
        self.field_index = Signal(64)
        self.field_found = Signal()
        self.field_inverted = Signal()
        self.field_freq = Signal(32)
        self.field_rate = Signal(32)
        self.field_gain = Signal(12)
        self.field_sync = Signal(signed(16))
        self.field_tracking = Signal()
        self.field_lost = Signal()

        self.counting = Signal()
        self.fields_counted = Signal()
        self.tracking = Signal()
        self.overrun = Signal()

        self.mf = MatchedFilter(TAG_WIDTH)
        self.dp = Datapath(TAG_WIDTH)

    def elaborate(self, platform):
        m = Module()
        m.submodules.mf = mf = self.mf
        m.submodules.dp = dp = self.dp
        m.d.comb += [
            mf.wr_en.eq(self.wr_en), mf.wr_re.eq(self.wr_re), mf.wr_im.eq(self.wr_im),
            mf.tw_en.eq(self.tw_en), mf.tw_addr.eq(self.tw_addr), mf.tw_data.eq(self.tw_data),
            dp.carrier0.eq(self.carrier0),
            dp.in_valid.eq(mf.out_valid), dp.in_re.eq(mf.out_re), dp.in_im.eq(mf.out_im),
            dp.in_kind.eq(mf.out_tag[:2]), dp.in_tag.eq(mf.out_tag),
        ]

        # ── The sync logic's answers to the scheduler, kept until it takes them ──────────
        timing_valid = Signal()
        timing_pending = Signal()
        timing = Signal(signed(17))
        search_done = Signal()
        search_next = Signal(10)
        # The error's moves of the instant and of the rate's sum, shifted as the gear says (the
        # gear cannot change before the scheduler takes them: it moves at a segment's end).
        timing_move = Signal(signed(17 + TIMING_KP[0]))
        timing_rate = Signal(signed(17 + TIMING_RATE_SHIFT[0]))
        acquiring = Signal(init=1)
        with m.If(timing_valid):
            m.d.sync += [
                timing_pending.eq(1),
                timing_move.eq(Mux(acquiring, timing << TIMING_KP[0], timing << TIMING_KP[1])),
                timing_rate.eq(Mux(acquiring, timing << TIMING_RATE_SHIFT[0],
                                   timing << TIMING_RATE_SHIFT[1])),
            ]

        # ── The scheduler ────────────────────────────────────────────────────────────────
        # 64.32: the next main symbol's instant, from where the filter's inputs begin.
        pos = Signal(96, init=(HALF << 32) + (1 << 31))
        rate_sum = Signal(signed(RATE_SUM_WIDTH))
        n = Signal(32)
        phase = Signal(2)
        kk = Signal(10)
        segs = Signal(range(ACQUIRE_SEGMENTS + 1))   # segments counted, saturating
        slot = Signal(2)
        delta = Signal(signed(40))
        # The step and its half from the rate, a cycle behind it (the rate moves only at the
        # fourth sync symbol, and the step taken there is the one before it moves).
        rate = Signal(signed(RATE_SUM_WIDTH - RATE_FRAC))
        m.d.comb += rate.eq(rate_sum >> RATE_FRAC)
        step = Signal(signed(34))
        half_step = Signal(signed(33))
        m.d.sync += [step.eq(self.step0 + rate), half_step.eq((self.step0 + rate) >> 1)]
        # The evaluation's offset (half a step back or on for early and late), its instant, its
        # input sample, how far the input leads it and whether its last input is in, a step each.
        offset = Signal(signed(34))
        eval_pos = Signal(64)
        i_eval = Signal(32)
        lead = Signal(signed(32))
        available = Signal()
        m.d.sync += [lead.eq(mf.in_count - i_eval), available.eq(lead >= HALF + 1)]
        with m.If(mf.issue & mf.ready & (lead > SAFE_LAG)):
            m.d.sync += self.overrun.eq(1)
        sync_symbol = (phase == PHASE_COUNT) & (kk < 4)
        # Flags of the symbol count, a cycle behind it (it moves only between issues).
        last_search = Signal()
        settle_end = Signal()
        m.d.sync += [acquiring.eq(segs < ACQUIRE_SEGMENTS),
                     last_search.eq(n == self.settle + self.search_segments * SEGMENT - 1),
                     settle_end.eq(n == self.settle - 1)]
        # The instant's high half takes the low half's carry a cycle later.
        carry = Signal(signed(2))

        # The main symbols' instants and the timing rate, in issue order, for the sync logic: each
        # pushed the cycle after its issue, when neither has moved yet.
        fifo_pos = Array(Signal(96, name=f'fifo_pos{i}') for i in range(4))
        fifo_rate = Array(Signal(signed(32), name=f'fifo_rate{i}') for i in range(4))
        fifo_w = Signal(2)
        fifo_r = Signal(2)
        push = Signal()
        m.d.sync += push.eq(0)
        with m.If(push):
            m.d.sync += [fifo_pos[fifo_w].eq(pos), fifo_rate[fifo_w].eq(rate),
                         fifo_w.eq(fifo_w + 1)]

        m.d.comb += [
            mf.issue_pos.eq(eval_pos),
            mf.issue_tag.eq(Cat(slot, phase, kk, n)),
        ]

        low = Signal(signed(50))
        m.d.comb += low.eq(pos[:48] + delta)

        def advance(new_phase, new_kk):
            return [pos[:48].eq(low[:48]), carry.eq(low[48:]), n.eq(n + 1), phase.eq(new_phase),
                    kk.eq(new_kk),
                    slot.eq(Mux((new_phase == PHASE_COUNT) & (new_kk < 4), KIND_EARLY, KIND_MAIN))]

        with m.FSM():
            with m.State('OFFSET'):
                m.d.sync += offset.eq(Mux(slot == KIND_EARLY, -half_step,
                                          Mux(slot == KIND_LATE, half_step, 0)))
                m.next = 'INSTANT'
            with m.State('INSTANT'):
                m.d.sync += eval_pos.eq(pos[:64] + offset)
                m.next = 'SAMPLE'
            with m.State('SAMPLE'):
                m.d.sync += i_eval.eq((eval_pos + (1 << (32 - 6 - 1)))[32:64])
                m.next = 'LEAD'
            with m.State('LEAD'):
                # `lead` follows the new sample from the next cycle, `available` the cycle after.
                m.next = 'READY'
            with m.State('READY'):
                m.next = 'ISSUE'
            with m.State('ISSUE'):
                m.d.comb += mf.issue.eq(available)
                # The next instant's step, unless the timing error moves it (the step does not
                # move while the scheduler waits here).
                m.d.sync += delta.eq(step)
                with m.If(available & mf.ready):
                    with m.If(slot == KIND_EARLY):
                        m.d.sync += slot.eq(KIND_LATE)
                        m.next = 'OFFSET'
                    with m.Elif(slot == KIND_LATE):
                        m.d.sync += slot.eq(KIND_MAIN)
                        m.next = 'OFFSET'
                    with m.Else():
                        m.d.sync += push.eq(1)
                        with m.If(sync_symbol & (kk == 3)):
                            m.next = 'TIMING'
                        with m.Elif((phase == PHASE_SEARCH) & last_search):
                            m.next = 'SEARCH'
                        with m.Else():
                            m.next = 'ADVANCE'
            with m.State('TIMING'):
                with m.If(timing_pending):
                    m.d.sync += [
                        timing_pending.eq(0),
                        delta.eq(step + timing_move),
                        rate_sum.eq(rate_sum + timing_rate),
                    ]
                    m.next = 'ADVANCE'
            with m.State('SEARCH'):
                with m.If(search_done):
                    m.d.sync += advance(PHASE_COUNT, search_next)
                    m.next = 'WAIT'
            with m.State('ADVANCE'):
                with m.If(phase == PHASE_COUNT):
                    wrap = kk == SEGMENT - 1
                    m.d.sync += advance(PHASE_COUNT, Mux(wrap, 0, kk + 1))
                    with m.If(wrap & acquiring):
                        m.d.sync += segs.eq(segs + 1)
                with m.Elif((phase == PHASE_SETTLE) & settle_end):
                    m.d.sync += advance(PHASE_SEARCH, 0)
                with m.Else():
                    m.d.sync += advance(phase, 0)
                m.next = 'WAIT'
            with m.State('WAIT'):
                # The instant's high half; the step follows a new rate.
                m.d.sync += pos[48:].eq(pos[48:] + carry)
                m.next = 'OFFSET'

        # ── The sync logic's input stage: the datapath's output, registered ──────────────
        pn = Array(C(int(b < 0)) for b in pn511())
        pn_mid = Array(C(int(b < 0)) for b in pn63())
        d_kind = dp.out_tag[:2]
        d_kk = dp.out_tag[4:14]
        o_valid = Signal()
        o_kind = Signal(2)
        o_phase = Signal(2)
        o_kk = Signal(10)
        o_n = Signal(32)
        yl = Signal(signed(Y_WIDTH))
        neg_y = Signal(signed(Y_WIDTH + 1))
        o_freq = Signal(signed(32))
        o_gain = Signal(12)
        o_dc = Signal(signed(16))
        o_re_in = Signal(signed(13))
        o_im_in = Signal(signed(13))
        o_gained = [Signal(signed(12), name=f'o_gained{i}') for i in range(2)]
        o_theta = Signal(32)
        o_turned = [Signal(signed(12), name=f'o_turned{i}') for i in range(2)]
        o_pos = Signal(96)
        o_rate = Signal(signed(32))
        sync_neg = Signal()      # the segment sync's sign at the place is -1
        pn_neg = Signal()
        mid_neg = Signal()
        m.d.sync += o_valid.eq(dp.out_valid)
        with m.If(dp.out_valid):
            m.d.sync += [
                o_kind.eq(d_kind), o_phase.eq(dp.out_tag[2:4]), o_kk.eq(d_kk),
                o_n.eq(dp.out_tag[14:46]), yl.eq(dp.out_y), neg_y.eq(-dp.out_y),
                o_freq.eq(dp.out_freq), o_gain.eq(dp.out_gain), o_dc.eq(dp.out_dc),
                o_re_in.eq(dp.out_re_in), o_im_in.eq(dp.out_im_in),
                o_gained[0].eq(dp.out_gained_re), o_gained[1].eq(dp.out_gained_im),
                o_theta.eq(dp.out_theta),
                o_turned[0].eq(dp.out_turned_re), o_turned[1].eq(dp.out_turned_im),
                sync_neg.eq(Array(C(s < 0) for s in SEGMENT_SYNC)[d_kk[:2]]),
                pn_neg.eq(pn[d_kk - 4]), mid_neg.eq(pn_mid[d_kk - MIDDLE_PN63]),
            ]
            # A main symbol's instant and rate, popped in order.
            with m.If(d_kind == KIND_MAIN):
                m.d.sync += [o_pos.eq(fifo_pos[fifo_r]), o_rate.eq(fifo_rate[fifo_r]),
                             fifo_r.eq(fifo_r + 1)]
        main = o_valid & (o_kind == KIND_MAIN)
        side = o_valid & (o_kind != KIND_MAIN)
        signed_y = Mux(sync_neg, neg_y, yl)

        # ── Segment search: the sums in block RAM ────────────────────────────────────────
        m.submodules.search = search = Memory(shape=signed(24), depth=1024, init=[])
        srd = search.read_port(transparent_for=())
        swr = search.write_port()
        s_count = Signal(range(self.search_segments * SEGMENT + 1))
        s_bin = Signal(10)
        last = [Signal(signed(Y_WIDTH), name=f'last{i}') for i in range(2)]
        # The sync's correlation ending at a symbol is last0 - last1 - last2 + y, where last0-2 are
        # the three symbols before; their part is kept ready.
        part = Signal(signed(15))
        searching = main & (o_phase == PHASE_SEARCH)
        # Step 1: the term for the bin, the bin's read; step 2: the read registered; step 3: the
        # sum; step 4: the sum written back, the best bin of the last pass followed.
        s1 = dict(go=Signal(name='s1_go'), term=Signal(signed(16), name='s1_term'),
                  bin=Signal(10, name='s1_bin'), first=Signal(name='s1_first'),
                  final=Signal(name='s1_final'), lastone=Signal(name='s1_lastone'))
        s2 = {k: Signal.like(v, name=f's2_{k}') for k, v in s1.items()}
        s3 = {k: Signal.like(v, name=f's3_{k}') for k, v in s1.items() if k != 'term'}
        m.d.comb += [srd.addr.eq(s_bin), srd.en.eq(1)]
        m.d.sync += s1['go'].eq(searching)
        with m.If(searching):
            m.d.sync += [
                last[0].eq(last[1]), last[1].eq(yl), part.eq(last[0] - last[1] - yl),
                s1['term'].eq(part + yl), s1['bin'].eq(s_bin),
                s1['first'].eq(s_count < SEGMENT),
                s1['final'].eq(s_count >= (self.search_segments - 1) * SEGMENT),
                s1['lastone'].eq(s_count == self.search_segments * SEGMENT - 1),
                s_count.eq(s_count + 1),
                s_bin.eq(Mux(s_bin == SEGMENT - 1, 0, s_bin + 1)),
            ]
        read = Signal(signed(24))
        m.d.sync += [read.eq(srd.data), *[s2[k].eq(v) for k, v in s1.items()]]
        s_sum = Signal(signed(24))
        m.d.sync += [s_sum.eq(Mux(s2['first'], 0, read) + s2['term']),
                     *[s3[k].eq(s2[k]) for k in s3]]
        m.d.comb += [swr.en.eq(s3['go']), swr.addr.eq(s3['bin']), swr.data.eq(s_sum)]
        best = Signal(10)
        best_val = Signal(signed(24))
        with m.If(s3['go']):
            with m.If(s3['final'] & ((s_sum >= best_val) | (s3['bin'] == 0))):
                m.d.sync += [best.eq(s3['bin']), best_val.eq(s_sum)]
            with m.If(s3['lastone']):
                m.d.sync += search_done.eq(1)
        # Bin `best` holds the sync's last symbol (place 3); the next symbol after the search's
        # last (bin 831) is at place (2 x 832 + 3 - best) mod 832.
        m.d.comb += search_next.eq(Mux(best <= 3, 3 - best, SEGMENT + 3 - best))

        # ── The counted segments' correlations ───────────────────────────────────────────
        acc_sync = Signal(signed(16))
        acc_early = Signal(signed(16))
        acc_late = Signal(signed(16))
        acc_pn = Signal(signed(22))
        acc_mid = Signal(signed(20))
        seg_first_pos = Signal(96)
        seg_index = Signal(range(1024))         # segments counted, saturating
        last_timing = Signal(signed(17))
        counting = o_phase == PHASE_COUNT
        m.d.sync += timing_valid.eq(0)
        with m.If(side & counting):
            with m.If(o_kind == KIND_EARLY):
                m.d.sync += acc_early.eq(acc_early + signed_y)
            with m.Else():
                m.d.sync += acc_late.eq(acc_late + signed_y)
                with m.If(o_kk == 3):
                    m.d.sync += [timing.eq(acc_late + signed_y - acc_early),
                                 last_timing.eq(acc_late + signed_y - acc_early),
                                 timing_valid.eq(1)]

        # At a segment's end its sums are kept for the next cycle, which takes the field
        # decisions; the next symbol is at least 16 cycles away.
        end = Signal()
        e_pn = Signal.like(acc_pn)
        e_mid_neg = Signal()
        e_sync = Signal.like(acc_sync)
        e_n = Signal(32)
        e_pos = Signal(96)
        e_freq = Signal(signed(32))
        e_rate = Signal(signed(32))
        e_gain = Signal(12)
        e_dc = Signal(signed(16))
        e_index = Signal.like(seg_index)
        m.d.sync += end.eq(0)
        with m.If(main & counting):
            with m.If(o_kk < 4):
                m.d.sync += acc_sync.eq(acc_sync + signed_y)
            with m.If(o_kk == 0):
                m.d.sync += seg_first_pos.eq(o_pos)
            with m.If((o_kk >= 4) & (o_kk < 4 + 511)):
                m.d.sync += acc_pn.eq(acc_pn + Mux(pn_neg, -yl, yl))
            with m.If((o_kk >= MIDDLE_PN63) & (o_kk < MIDDLE_PN63 + 63)):
                m.d.sync += acc_mid.eq(acc_mid + Mux(mid_neg, -yl, yl))
            with m.If(o_kk == SEGMENT - 1):
                m.d.sync += [
                    end.eq(1), e_pn.eq(acc_pn), e_mid_neg.eq(acc_mid < 0), e_sync.eq(acc_sync),
                    e_n.eq(o_n), e_pos.eq(seg_first_pos), e_freq.eq(o_freq), e_rate.eq(o_rate),
                    e_gain.eq(o_gain), e_dc.eq(o_dc), e_index.eq(seg_index),
                    acc_sync.eq(0), acc_early.eq(0), acc_late.eq(0), acc_pn.eq(0), acc_mid.eq(0),
                    seg_index.eq(Mux(seg_index == 1023, seg_index, seg_index + 1)),
                ]

        # Field state.
        f_counting = Signal()
        f_seen = Signal(9)
        f_best = Signal(signed(22))
        f_ago = Signal(9)
        f_inv = Signal()
        f_seg = Signal(9)
        f_misses = Signal(3)
        cur_seg = Signal(9)
        field_no = Signal(32, init=(1 << 32) - 1)

        # The comparisons a cycle before the decisions.
        decide = Signal()
        found = Signal()
        better = Signal()
        best_found = Signal()
        m.d.sync += [decide.eq(end), self.seg_valid.eq(end), self.field_valid.eq(0),
                     self.field_lost.eq(0)]
        with m.If(end):
            m.d.sync += [
                found.eq(e_pn > FIELD_SYNC_THRESHOLD), better.eq(e_pn > f_best),
                best_found.eq(Mux(e_pn > f_best, e_pn > FIELD_SYNC_THRESHOLD, f_best > FIELD_SYNC_THRESHOLD)),
                self.seg_start.eq(e_n + 1 - SEGMENT), self.seg_pos.eq(e_pos),
                self.seg_freq.eq(e_freq), self.seg_rate.eq(e_rate),
                self.seg_gain.eq(e_gain), self.seg_dc.eq(e_dc),
                self.seg_sync.eq(e_sync), self.seg_timing.eq(last_timing),
                self.seg_pn511.eq(e_pn),
            ]
        with m.If(decide):
            with m.If(~f_counting):
                ago = Mux(better, 0, f_ago + 1)
                best_pn = Mux(better, e_pn, f_best)
                inv = Mux(better, e_mid_neg, f_inv)
                with m.If(f_seen == FIELD_SEGMENTS - 1):
                    with m.If(best_found):
                        m.d.sync += [f_counting.eq(1), f_seg.eq(ago), f_misses.eq(0),
                                     f_inv.eq(inv),
                                     cur_seg.eq(Mux(ago == FIELD_SEGMENTS - 1, 0, ago + 1))]
                        with m.If(ago == FIELD_SEGMENTS - 1):
                            m.d.sync += field_no.eq(field_no + 1)
                    with m.Else():
                        m.d.sync += [f_seen.eq(0), f_best.eq(0), f_ago.eq(0), f_inv.eq(0)]
                with m.Else():
                    m.d.sync += [f_seen.eq(f_seen + 1), f_best.eq(best_pn), f_ago.eq(ago),
                                 f_inv.eq(inv)]
            with m.Else():
                seg_next = Mux(f_seg == FIELD_SEGMENTS - 1, 0, f_seg + 1)
                m.d.sync += [f_seg.eq(seg_next),
                             cur_seg.eq(Mux(seg_next == FIELD_SEGMENTS - 1, 0, seg_next + 1))]
                with m.If(seg_next == FIELD_SEGMENTS - 1):
                    m.d.sync += field_no.eq(field_no + 1)
                with m.If(seg_next == 0):
                    misses = Mux(found, 0, f_misses + 1)
                    inv = Mux(found, e_mid_neg, ~f_inv)
                    m.d.sync += [f_misses.eq(misses), f_inv.eq(inv)]
                    with m.If(misses >= FIELD_MISSES):
                        m.d.sync += [f_counting.eq(0), f_seen.eq(0), f_best.eq(0),
                                     f_ago.eq(0), f_inv.eq(0), self.field_lost.eq(1)]
                    with m.Else():
                        m.d.sync += [
                            self.field_valid.eq(1),
                            self.field_no.eq(field_no),
                            self.field_start.eq(e_n + 1 - SEGMENT),
                            self.field_index.eq(e_pos[32:]),
                            self.field_found.eq(found), self.field_inverted.eq(inv),
                            self.field_freq.eq(e_freq), self.field_rate.eq(e_rate),
                            self.field_gain.eq(e_gain), self.field_sync.eq(e_sync),
                            self.field_tracking.eq(e_index > ACQUIRE_SEGMENTS),
                        ]

        # ── The outputs ──────────────────────────────────────────────────────────────────
        m.d.sync += [self.y_valid.eq(main), self.side_valid.eq(side)]
        with m.If(main):
            m.d.sync += [
                self.y.eq(yl), self.y_phase.eq(o_phase), self.y_place.eq(o_kk),
                self.y_fields.eq(f_counting & counting), self.y_segment.eq(cur_seg),
                self.y_field.eq(field_no), self.y_n.eq(o_n), self.y_pos.eq(o_pos),
                self.t_filtered[0].eq(o_re_in), self.t_filtered[1].eq(o_im_in),
                self.t_gain.eq(o_gain),
                self.t_gained[0].eq(o_gained[0]), self.t_gained[1].eq(o_gained[1]),
                self.t_theta.eq(o_theta),
                self.t_turned[0].eq(o_turned[0]), self.t_turned[1].eq(o_turned[1]),
                self.t_dc.eq(o_dc),
            ]
        with m.If(side):
            m.d.sync += [
                self.side_late.eq(o_kind == KIND_LATE), self.side_n.eq(o_n),
                self.side_filtered[0].eq(o_re_in), self.side_filtered[1].eq(o_im_in),
                self.side_theta.eq(o_theta), self.side_value.eq(yl),
            ]
        m.d.comb += [
            self.counting.eq(phase == PHASE_COUNT),
            self.fields_counted.eq(f_counting),
            self.tracking.eq(~acquiring),
        ]
        return m
