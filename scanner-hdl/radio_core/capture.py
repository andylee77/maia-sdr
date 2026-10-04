#
# Fishball radio core - the raw IQ capture: the AD9361's samples, two to a 64-bit word
# (iq_packer.py), round a ring of sub-buffers in DDR, with the sample index of its first sample
# and a stop after a trigger (doc/changes/082, item 7).
#
# Each enable (0 to 1) starts the ring afresh, once the writes of the capture before have
# finished: at its first sub-buffer, with `last_buffer` at -1, and with `start_index` set to the
# sample index of the first sample it writes. The buffer count reads 0 from the enable on. While `overflow`
# stays clear, the k-th sub-buffer since the enable (k = 0, 1, ...; slot k mod the ring's
# sub-buffers) begins with sample `start_index + k * samples per sub-buffer`.
#
# A trigger (a write pulse) lets the capture finish the sub-buffer it is writing and
# `stop_after` more, then stop at that sub-buffer's end: no address past it is issued, so the
# older sub-buffers keep what they hold. `stopped` says it has stopped; the next enable starts
# it again.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *

from .dma_ring import DmaStreamRingWrite
from .iq_packer import IQPacker


class Capture(Elaboratable):
    """The capture ring and its packer.

    Parameters
    ----------
    base_address, num_buffers_log2, buffer_size : int
        The ring (``DmaStreamRingWrite``'s parameters).
    name : str
        The AXI manager's name.

    Inputs (sync):
        re_in, im_in : signed(16)   a sample, valid with ``strobe_in``
        strobe_in
        sample_index : Signal(64)   the sample's index (the AD9361 sample count)
        enable                      a rise starts a capture; a fall stops it
        trigger                     one cycle: stop after ``stop_after`` more sub-buffers
        stop_after : Signal(num_buffers_log2)

    Outputs (sync):
        overflow                    one cycle: a word was lost while the capture ran
        start_index : Signal(64)    the index of the capture's first sample
        buffers : Signal(32)        sub-buffers completed since the enable
        stopped                     the capture stopped itself after a trigger
        last_buffer, interrupt      the ring's (``DmaStreamRingWrite``)
    """
    def __init__(self, base_address, num_buffers_log2, buffer_size, *, name):
        self.packer = IQPacker()
        self.dma = DmaStreamRingWrite(base_address, num_buffers_log2, buffer_size,
                                      width=64, axi_awidth=32, name=name)
        # 128-byte bursts in a sub-buffer.
        self._bursts_log2 = (buffer_size // 128).bit_length() - 1
        assert 128 << self._bursts_log2 == buffer_size

        self.re_in = Signal(signed(16))
        self.im_in = Signal(signed(16))
        self.strobe_in = Signal()
        self.sample_index = Signal(64)
        self.enable = Signal()
        self.trigger = Signal()
        self.stop_after = Signal(num_buffers_log2)

        self.overflow = Signal()
        self.start_index = Signal(64)
        self.buffers = Signal(32)
        self.stopped = Signal()
        self.last_buffer = self.dma.last_buffer
        self.interrupt = self.dma.interrupt

    def elaborate(self, platform):
        m = Module()
        m.submodules.packer = packer = self.packer
        m.submodules.dma = dma = self.dma
        axi = dma.axi

        # The enable's rise is held until the capture before has finished its writes.
        enable_q = Signal()
        start_pending = Signal()
        running = Signal()
        m.d.sync += enable_q.eq(self.enable)
        with m.If(self.enable & ~enable_q):
            # The count and `stopped` describe the new capture from the enable on, before the
            # ring has started again.
            m.d.sync += [start_pending.eq(1), self.buffers.eq(0), self.stopped.eq(0)]
        with m.If(~self.enable):
            m.d.sync += [start_pending.eq(0), running.eq(0)]

        # An AW presented stays presented until it is taken (AXI), whatever the enable does.
        aw_waiting = Signal()
        m.d.sync += aw_waiting.eq(axi.awvalid & ~axi.awready)

        restart = Signal()
        m.d.comb += restart.eq(start_pending & self.enable & dma.idle & ~aw_waiting)
        first_pending = Signal()
        with m.If(restart):
            m.d.sync += [start_pending.eq(0), running.eq(1), first_pending.eq(1)]
        with m.Elif(first_pending & self.strobe_in):
            m.d.sync += [first_pending.eq(0), self.start_index.eq(self.sample_index)]

        # AWs issued since the restart: the sub-buffer and the burst in it of the next one.
        aw_buffer = Signal(32)
        aw_burst = Signal(self._bursts_log2)
        with m.If(restart):
            m.d.sync += [aw_buffer.eq(0), aw_burst.eq(0)]
        with m.Elif(axi.aw_handshake()):
            m.d.sync += aw_burst.eq(aw_burst + 1)
            with m.If(aw_burst == (1 << self._bursts_log2) - 1):
                m.d.sync += aw_buffer.eq(aw_buffer + 1)

        # The trigger: the sub-buffer of the AW last presented (taken or waiting) is the one being
        # written; the capture ends `stop_after` sub-buffers after it.
        triggered = Signal()
        stop_at = Signal(32)
        current = Signal(32)
        m.d.comb += current.eq(Mux(axi.awvalid | (aw_burst != 0) | (aw_buffer == 0),
                                   aw_buffer, aw_buffer - 1))
        with m.If(restart):
            m.d.sync += triggered.eq(0)
        with m.Elif(running & self.trigger & ~triggered):
            m.d.sync += [triggered.eq(1), stop_at.eq(current + self.stop_after)]
        closed = Signal()
        m.d.comb += closed.eq(triggered & (aw_buffer > stop_at))
        with m.If(running & closed & dma.idle & ~aw_waiting):
            m.d.sync += self.stopped.eq(1)

        with m.If(running & dma.interrupt):
            m.d.sync += self.buffers.eq(self.buffers + 1)

        m.d.comb += [
            dma.restart.eq(restart),
            dma.enable.eq((running & ~closed) | aw_waiting),
            dma.stream_data.eq(packer.data_out),
            dma.stream_valid.eq(packer.data_valid),
            packer.stream_ready.eq(dma.stream_ready),
            packer.re_in.eq(self.re_in),
            packer.im_in.eq(self.im_in),
            packer.strobe_in.eq(self.strobe_in),
            # Between captures the packer holds nothing: a capture's first word is its first two
            # samples, and words no capture will take are not counted as lost.
            packer.clear.eq(restart | (~running & dma.idle) | self.stopped),
            self.overflow.eq(packer.overflow & running & ~self.stopped),
        ]
        return m
