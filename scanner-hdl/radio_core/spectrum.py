#
# Fishball radio core - the wideband spectrometer, framed (doc/changes/082, items 4-6).
#
# maia_hdl's Spectrometer, rebuilt from its blocks: the 4096-point FFT, the integrator (here
# FrameIntegrator, which takes its mode per frame), and the DMA that writes each finished frame
# to the next sub-buffer of a ring. Each frame carries a header in its edge bins, which lie at
# -fs/2, outside the +-0.45 x rate that every user of the spectrum reads: the DMA's read side
# puts the header's words in place of bins 0-7, and a check word in place of bin 4095.
#
# A frame is 4096 words of 64 bits, bin 0 at -fs/2. A bin holds the integrator's 47-bit mantissa
# in [46:0] and its exponent in [58:56] (powers of 4), as Maia's. The header:
#
#   word 0   [15:0] magic 0x5346 ("SF"), [19:16] format (1), [31:24] flags: bit 0 `peak` (the
#            frame is peak hold, else average power), bit 1 `aborted` (an abort ended it before
#            its integrations were done); [41:32] the transforms it integrated
#   word 1   the AD9361 sample index of its first transform's first input sample
#   word 2   [31:0] sequence (frames since the core's reset, wraps), [63:32] the running count of
#            AD9361 samples at full scale when that first sample was taken
#   3-7      zero
#   bin 4095 [31:0] check: the XOR of the frame's other 32-bit halves; [63:32] zero
#
# SPDX-License-Identifier: MIT
#

from amaranth import *

from maia_hdl.dma import DmaBRAMWrite
from maia_hdl.fft import FFT, TwiddleI

from .frame_integrator import FrameIntegrator

MAGIC = 0x5346
FORMAT = 1
ORDER_LOG2 = 12
HEADER_WORDS = 8

FLAG_PEAK = 1 << 0
FLAG_ABORTED = 1 << 1


def fold(word):
    """XOR of a 64-bit word's two 32-bit halves."""
    return (word ^ (word >> 32)) & 0xFFFF_FFFF


def header_words(*, peak, aborted, transforms, sample_index, sequence, adc_clips):
    """A frame's 8 header words (Python reference)."""
    flags = (FLAG_PEAK if peak else 0) | (FLAG_ABORTED if aborted else 0)
    return [
        MAGIC | (FORMAT << 16) | (flags << 24) | (transforms << 32),
        sample_index & (2**64 - 1),
        (sequence & 0xFFFF_FFFF) | ((adc_clips & 0xFFFF_FFFF) << 32),
        0, 0, 0, 0, 0,
    ]


def bin_word(value, exponent):
    """A bin as the DMA writes it (Python reference)."""
    return value | (exponent << 56)


def frame_words(header, bins):
    """A frame as the DMA writes it (Python reference): `bins` with the header in bins 0-7
    and the check in the last bin."""
    words = list(header) + list(bins[HEADER_WORDS:-1])
    check = 0
    for w in words:
        check ^= fold(w)
    return words + [check]


def _fold(word):
    return word[:32] ^ word[32:]


class TwiddleEdgeFFT(FFT):
    """maia_hdl's FFT with a 3x common edge for each twiddle multiplier
    (``common_edge_twiddles``) in place of the one ``common_edge_3x`` they share.

    The twiddle multipliers are spread along the FFT; with one edge, synthesis also merged their
    equal copies of the registers derived from it, so one register drove every multiplier's
    multiplexers across the die. The parameters and the rest are ``FFT``'s; ``elaborate`` is
    upstream's with the edges wired one to a multiplier.
    """
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.multipliers = [t for t in self._twiddles if not isinstance(t, TwiddleI)]
        self.common_edge_twiddles = [Signal(name=f'common_edge_twiddle{j}')
                                     for j in range(len(self.multipliers))]

    def elaborate(self, platform):
        m = Module()
        if self._window is not None:
            m.submodules.window = self._window
            m.d.comb += self._window.common_edge.eq(self.common_edge_2x)
            first = self._window
        else:
            first = self._butterflies[0]
        for j, bfly in enumerate(self._butterflies):
            m.submodules[f'bfly{j}'] = bfly
        for j, twiddle in enumerate(self._twiddles):
            m.submodules[f'twiddle{j}'] = twiddle
        m.d.comb += [t.common_edge.eq(edge)
                     for t, edge in zip(self.multipliers, self.common_edge_twiddles)]
        m.submodules.control = ctrl = self._control
        ctrl.connect_stages(m)
        last_bfly = self._butterflies[-1]
        m.d.comb += [
            ctrl.clken.eq(self.clken),
            first.re_in.eq(self.re_in),
            first.im_in.eq(self.im_in),
            self.re_out.eq(last_bfly.re_out),
            self.im_out.eq(last_bfly.im_out),
            self.out_last.eq(ctrl.out_last),
        ]
        return m


class SpectrumFrames(Elaboratable):
    """The framed spectrometer.

    Parameters
    ----------
    dma_base_address, dma_buffers_log2 : int
        The ring (a frame per sub-buffer).
    dma_name : str
    domain_2x, domain_3x : str
    order_log2 : int
        The FFT's size (4096 points in the core; smaller in tests).

    Inputs (sync):
        re_in, im_in : signed(16)       a sample, valid with ``strobe_in``
        strobe_in
        sample_index : Signal(64)       the sample's index (the AD9361 sample count)
        adc_clips : Signal(32)          the running count of samples at full scale
        number_integrations : Signal(10), peak_detect, alternate, abort
                                        the integrator's (``FrameIntegrator``)
        common_edge_2x                  the FFT window's
        common_edge_twiddles : [Signal()]
                                        each of the FFT's twiddle multipliers' own 3x common
                                        edge (one per multiplier, ``len(fft.multipliers)``)
        common_edge_integrator          the integrator's own 3x common edge

    Outputs (sync):
        last_buffer                     the last sub-buffer written whole
        interrupt_out                   one cycle when a frame has been written
    """
    def __init__(self, dma_base_address, dma_buffers_log2, *, dma_name,
                 domain_2x='clk2x', domain_3x='clk3x', order_log2=ORDER_LOG2):
        self.order_log2 = order_log2
        self.bins = 1 << order_log2
        self.width_in = 16
        self.nint_width = 10

        self.fft = TwiddleEdgeFFT(
            self.width_in, order_log2, 'R22',
            width_twiddle=16, truncates=[[0, 1]] * (order_log2 // 2),
            use_bram_reg=True, window='blackmanharris',
            cmult3x=True, domain_2x=domain_2x, domain_3x=domain_3x)
        self.integrator = FrameIntegrator(
            domain_3x, len(self.fft.re_out), 18, self.nint_width, order_log2)
        self.dma = DmaBRAMWrite(dma_base_address, dma_buffers_log2, order_log2, name=dma_name)

        self.re_in = Signal(signed(self.width_in))
        self.im_in = Signal(signed(self.width_in))
        self.strobe_in = Signal()
        self.sample_index = Signal(64)
        self.adc_clips = Signal(32)
        self.number_integrations = Signal(self.nint_width)
        self.peak_detect = Signal()
        self.alternate = Signal()
        self.abort = Signal()
        self.common_edge_2x = Signal()
        self.common_edge_twiddles = [Signal(name=f'common_edge_twiddle{j}')
                                     for j in range(len(self.fft.multipliers))]
        self.common_edge_integrator = Signal()

        self.last_buffer = Signal(dma_buffers_log2)
        self.interrupt_out = Signal()

    def elaborate(self, platform):
        m = Module()
        m.submodules.fft = fft = self.fft
        m.submodules.integrator = integrator = self.integrator
        m.submodules.dma = dma = self.dma
        order = self.order_log2
        check_bin = self.bins - 1

        m.d.comb += [
            fft.clken.eq(self.strobe_in),
            fft.common_edge_2x.eq(self.common_edge_2x),
            *[a.eq(b) for a, b in zip(fft.common_edge_twiddles, self.common_edge_twiddles)],
            fft.re_in.eq(self.re_in),
            fft.im_in.eq(self.im_in),

            integrator.nint.eq(self.number_integrations),
            integrator.abort.eq(self.abort),
            integrator.peak_detect.eq(self.peak_detect),
            integrator.alternate.eq(self.alternate),
            integrator.clken.eq(self.strobe_in),
            integrator.common_edge.eq(self.common_edge_integrator),
            integrator.input_last.eq(fft.out_last),
            integrator.re_in.eq(fft.re_out),
            integrator.im_in.eq(fft.im_out),
            integrator.rdaddr.eq(dma.raddr),
            integrator.rden.eq(dma.ren),
        ]

        # ── Each transform's first input sample: its index and the clip count then ─────────
        # The FFT takes input slot 0 of a transform every 4096th sample since its reset, and
        # puts out output slot 0 `fft.delay` samples later. The indices taken at the last few
        # input slots 0 are kept; when a frame starts at the integrator, the one of its first
        # transform is `fft.delay` samples old.
        delay = fft.delay
        assert delay >= 2
        back = (delay - 2) // self.bins
        slot = Signal(order)
        hist_index = [Signal(64, name=f'slot0_index{j}') for j in range(back + 1)]
        hist_clips = [Signal(32, name=f'slot0_clips{j}') for j in range(back + 1)]
        with m.If(self.strobe_in):
            m.d.sync += slot.eq(slot + 1)
            with m.If(slot == 0):
                m.d.sync += [
                    hist_index[0].eq(self.sample_index),
                    hist_clips[0].eq(self.adc_clips),
                    *[hist_index[j].eq(hist_index[j - 1]) for j in range(1, back + 1)],
                    *[hist_clips[j].eq(hist_clips[j - 1]) for j in range(1, back + 1)],
                ]
        frame_index = Signal(64)
        frame_clips = Signal(32)
        closed_index = Signal(64)
        closed_clips = Signal(32)
        with m.If(integrator.frame_start):
            m.d.sync += [
                frame_index.eq(hist_index[back]),
                frame_clips.eq(hist_clips[back]),
                closed_index.eq(frame_index),
                closed_clips.eq(frame_clips),
            ]

        # ── The frame's header, fixed when the DMA starts on it ─────────────────────────────
        sequence = Signal(32)
        h_peak = Signal()
        h_aborted = Signal()
        h_transforms = Signal(self.nint_width)
        h_index = Signal(64)
        h_seq = Signal(32)
        h_clips = Signal(32)
        with m.If(integrator.done):
            m.d.sync += [
                h_peak.eq(integrator.closed_peak),
                h_aborted.eq(integrator.closed_aborted),
                h_transforms.eq(integrator.closed_transforms),
                h_index.eq(closed_index),
                h_seq.eq(sequence),
                h_clips.eq(closed_clips),
                sequence.eq(sequence + 1),
            ]
        header = Array([
            Cat(C(MAGIC, 16), C(FORMAT, 4), C(0, 4), h_peak, h_aborted, C(0, 6),
                h_transforms, C(0, 32 - self.nint_width)),
            h_index,
            Cat(h_seq, h_clips),
            *[C(0, 64)] * (HEADER_WORDS - 3),
        ])

        # ── The DMA's read side: the header, the bins, the check ───────────────────────────
        # The DMA writes bin k on its k-th W beat, from data held until the beat is taken.
        beat = Signal(order)
        check = Signal(32)
        bin_data = Cat(integrator.rdata_value,
                       C(0, 56 - len(integrator.rdata_value)),
                       integrator.rdata_exponent,
                       C(0, 8 - len(integrator.rdata_exponent)))
        assert len(bin_data) == 64
        word = Signal(64)
        m.d.comb += word.eq(
            Mux(beat < HEADER_WORDS, header[beat[:3]],
                Mux(beat == check_bin, Cat(check, C(0, 32)), bin_data)))
        with m.If(integrator.done):
            m.d.sync += [beat.eq(0), check.eq(0)]
        with m.Elif(dma.axi.w_handshake()):
            m.d.sync += [beat.eq(beat + 1), check.eq(check ^ _fold(word))]

        dma_busy_q = Signal()
        m.d.sync += dma_busy_q.eq(dma.busy)
        m.d.comb += [
            dma.rdata.eq(word),
            dma.start.eq(integrator.done),
            self.last_buffer.eq(dma.last_buffer),
            self.interrupt_out.eq(~dma.busy & dma_busy_q),
        ]
        return m
