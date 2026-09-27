#
# Fishball hwval - instrumented replica of the production wideband ring
#
# p25_hdl.IQPacker -> maia_hdl.DmaStreamRingWrite, instantiated exactly as
# p25_top.py does for wideband_iq, with non-invasive counters on the
# stream and AXI handshakes (doc/HW_VALIDATION_SUITE.md sections 6.4 and
# findings F5/F6).
#
# SPDX-License-Identifier: MIT
#

from amaranth import *

from maia_hdl.dma import DmaStreamRingWrite
from p25_hdl.iq_packer import IQPacker

from .lat_hist import LatencyTracker


def _sat_inc(sig):
    """Saturating +1 of an unsigned signal."""
    return Mux(sig.all(), sig, sig + 1)


class LegacyRing(Elaboratable):
    """Production wideband ring replica with observation taps.

    The packer and the DMA are the unmodified production modules, wired
    as in ``p25_top.py`` (``wideband_iq_packer`` ->
    ``wideband_iq_dma``: stream_data/valid into the DMA, stream_ready
    back to the packer, ``enable`` straight into the DMA). Only their
    signals are observed.

    ``re_in``/``im_in`` feed the packer's signed 16-bit inputs bit for
    bit. A 12-bit live source must be sign-extended first, as
    ``p25_top`` does: ``legacy.re_in.eq(re12.as_signed())``.

    Counters (32-bit saturating, reset by ``clear``):

    - ``packer_ovf`` / ``packer_ovf_disabled``: packer overflow pulses
      while ``enable`` is high / low. Each pulse is one 64-bit word
      (2 samples) overwritten in the packer's holding register. While the
      DMA is disabled the packer overwrites every word, so
      ``packer_ovf_disabled`` measures disabled time, not loss.
    - ``words_in``: words formed by the packer (second strobe of a pair;
      the packer phase is replicated from ``strobe_in``, which toggles it
      on every strobe from reset).
    - ``words_accepted``: stream handshakes (identical to W handshakes).
    - ``aw_count``, ``b_count``, ``bresp_err`` (B with BRESP != OKAY).
    - ``subbuf_done``: DMA ``interrupt`` pulses (sub-buffers completed).
    - ``stall_cycles`` / ``max_stall``: cycles, and longest run, of
      ``stream_valid & ~stream_ready`` while ``enable`` is high.

    ``max_outstanding``, ``lat_max`` and ``hist_sel``/``hist_val`` come
    from a ``LatencyTracker`` on WLAST handshake -> B handshake, i.e. the
    write-response latency that sets the F5 cliff, and the number of
    bursts waiting for their response. The DMA stops accepting stream
    words when 5 bursts wait, so ``max_outstanding == 5`` means the ring
    reached its cap. (AW -> B would mostly measure the stream filling
    the burst: the DMA issues AW up to two bursts before the data exists,
    about 8 us at 8 MSPS even with an ideal interconnect.)

    Parameters
    ----------
    base : int
        Ring base address (aligned to the ring size).
    num_buffers_log2 : int
        log2 of the number of sub-buffers.
    buffer_size : int
        Sub-buffer size in bytes.
    name : str
        Name prefix of the AXI3 manager port.

    Attributes
    ----------
    axi : AXI3 manager interface of the DMA.
    re_in, im_in : Signal(16), in
    strobe_in : Signal(), in
    enable : Signal(), in
        DMA enable (production ``wideband_iq_enable``).
    clear : Signal(), in
        Pulse: clear the counters.
    last_buffer : Signal(num_buffers_log2), out
    next_address : Signal(32), out
        DMA AWADDR (production ``wideband_iq_next_address``).
    irq : Signal(), out
        DMA sub-buffer interrupt pulse.
    packer_ovf, packer_ovf_disabled, words_in, words_accepted, aw_count,
    b_count, bresp_err, subbuf_done, stall_cycles, max_stall,
    lat_max : Signal(32), out
    max_outstanding : Signal(range(17)), out
        High-water mark of bursts between WLAST and B (DMA cap: 5).
    hist_sel : Signal(4), in
    hist_val : Signal(32), out
    """
    def __init__(self, *, base=0x2200_0000, num_buffers_log2=4,
                 buffer_size=1 << 20, name='m_axi_legacy'):
        self.base = base
        self.num_buffers_log2 = num_buffers_log2
        self.num_buffers = 1 << num_buffers_log2
        self.buffer_size = buffer_size
        self.size = self.num_buffers * buffer_size

        self.packer = IQPacker()
        self.dma = DmaStreamRingWrite(
            base, num_buffers_log2, buffer_size,
            width=64, axi_awidth=32, name=name)
        self.axi = self.dma.axi

        self.re_in = Signal(16)
        self.im_in = Signal(16)
        self.strobe_in = Signal()
        self.enable = Signal()
        self.clear = Signal()

        self.last_buffer = Signal(num_buffers_log2)
        self.next_address = Signal(32)
        self.irq = Signal()
        self.packer_ovf = Signal(32)
        self.packer_ovf_disabled = Signal(32)
        self.words_in = Signal(32)
        self.words_accepted = Signal(32)
        self.aw_count = Signal(32)
        self.b_count = Signal(32)
        self.bresp_err = Signal(32)
        self.subbuf_done = Signal(32)
        self.stall_cycles = Signal(32)
        self.max_stall = Signal(32)
        self.max_outstanding = Signal(range(17))
        self.lat_max = Signal(32)
        self.hist_sel = Signal(4)
        self.hist_val = Signal(32)

    def elaborate(self, platform):
        m = Module()
        m.submodules.packer = packer = self.packer
        m.submodules.dma = dma = self.dma
        # WLAST -> B; the DMA never has more than 5 bursts in that state
        m.submodules.lat = lat = LatencyTracker(max_outstanding=16)
        a = dma.axi

        # Production wiring (p25_top.py, wideband_iq)
        m.d.comb += [
            packer.re_in.eq(self.re_in),
            packer.im_in.eq(self.im_in),
            packer.strobe_in.eq(self.strobe_in),
            dma.stream_data.eq(packer.data_out),
            dma.stream_valid.eq(packer.data_valid),
            packer.stream_ready.eq(dma.stream_ready),
            dma.enable.eq(self.enable),
            self.last_buffer.eq(dma.last_buffer),
            self.next_address.eq(a.awaddr),
            self.irq.eq(dma.interrupt),
        ]

        # ---- Observation taps
        phase = Signal()  # replica of the packer's pair phase
        with m.If(self.strobe_in):
            m.d.sync += phase.eq(~phase)

        stall = Signal()
        b_hs = Signal()
        wlast_hs = Signal()
        m.d.comb += [
            stall.eq(self.enable & packer.data_valid & ~dma.stream_ready),
            b_hs.eq(a.b_handshake()),
            wlast_hs.eq(a.w_handshake() & a.wlast),
        ]

        def count(sig, cond):
            with m.If(cond):
                m.d.sync += sig.eq(_sat_inc(sig))

        count(self.packer_ovf, packer.overflow & self.enable)
        count(self.packer_ovf_disabled, packer.overflow & ~self.enable)
        count(self.words_in, self.strobe_in & phase)
        count(self.words_accepted, packer.data_valid & dma.stream_ready)
        count(self.aw_count, a.aw_handshake())
        count(self.b_count, b_hs)
        count(self.bresp_err, b_hs & (a.bresp != 0))
        count(self.subbuf_done, dma.interrupt)
        count(self.stall_cycles, stall)

        stall_run = Signal(32)
        with m.If(stall):
            m.d.sync += stall_run.eq(_sat_inc(stall_run))
            with m.If(stall_run >= self.max_stall):
                m.d.sync += self.max_stall.eq(_sat_inc(stall_run))
        with m.Else():
            m.d.sync += stall_run.eq(0)

        m.d.comb += [
            lat.start.eq(wlast_hs),
            lat.done.eq(b_hs),
            lat.clear.eq(self.clear),
            lat.hist_sel.eq(self.hist_sel),
            self.hist_val.eq(lat.hist_val),
            self.lat_max.eq(lat.lat_max),
            self.max_outstanding.eq(lat.outstanding_max),
        ]

        with m.If(self.clear):
            m.d.sync += [
                c.eq(0) for c in (
                    self.packer_ovf, self.packer_ovf_disabled,
                    self.words_in, self.words_accepted, self.aw_count,
                    self.b_count, self.bresp_err, self.subbuf_done,
                    self.stall_cycles, self.max_stall)
            ]

        return m
