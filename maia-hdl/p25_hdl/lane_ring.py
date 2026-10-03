#
# Fishball P25 - the lanes' packets into one stream for the lane ring's DMA. Each waiting packet
# goes out whole, 512 words back to back, the lanes taken in turn; the DMA's back-pressure only
# pauses a packet, never interleaves two.
#
# The DMA (DmaStreamRingWrite) issues a burst's address before its data, and HP1's interconnect
# passes write data in address order, so an address left waiting on a slow stream would hold up
# the other masters' writes. A packet is whole in block RAM when it starts, so the ring lets the
# DMA address exactly that packet's bursts, and its enable takes effect between packets.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *

from .lane_packetizer import PACKET_WORDS

# The DMA's bursts are 16 words.
BURSTS_PER_PACKET = PACKET_WORDS // 16


class LaneRing(Elaboratable):
    """Round-robin of the lanes' packets onto a 64-bit stream.

    Parameters
    ----------
    packetizers : list of LanePacketizer
        The lanes, in lane order. This module drives their read side.

    Inputs (sync):
        enable                      packets start only while set; a packet in progress finishes

    Stream to the DMA (sync):
        stream_data : Signal(64), out
        stream_valid : Signal(), out
        stream_ready : Signal(), in
        dma_enable : Signal(), out   the DMA's enable: a started packet has bursts not yet
                                     addressed
        aw_accepted : Signal(), in   the DMA's AW handshake
    """
    def __init__(self, packetizers):
        self.lanes = packetizers
        self.enable = Signal()
        self.stream_data = Signal(64)
        self.stream_valid = Signal()
        self.stream_ready = Signal()
        self.dma_enable = Signal()
        self.aw_accepted = Signal()

    def elaborate(self, platform):
        m = Module()
        n = len(self.lanes)
        cur = Signal(range(n))
        active = Signal()
        addr = Signal(range(PACKET_WORDS + 1))   # the next word to read
        out_valid = Signal()                     # the lane's read register holds a word not yet sent

        ready = Array(p.ready for p in self.lanes)
        data = Array(p.rd_data for p in self.lanes)

        advance = Signal()
        m.d.comb += advance.eq(~out_valid | self.stream_ready)
        fetch = Signal()
        m.d.comb += fetch.eq(active & advance & (addr < PACKET_WORDS))
        for i, p in enumerate(self.lanes):
            m.d.comb += [
                p.rd_addr.eq(addr[:9]),
                p.rd_en.eq(fetch & (cur == i)),
            ]
        m.d.comb += [
            self.stream_data.eq(data[cur]),
            self.stream_valid.eq(out_valid),
        ]

        with m.If(fetch):
            m.d.sync += [addr.eq(addr + 1), out_valid.eq(1)]
        with m.Elif(self.stream_ready):
            m.d.sync += out_valid.eq(0)

        # The packet's last word was sent: release it and look for the next lane.
        done = active & (addr == PACKET_WORDS) & (~out_valid | self.stream_ready) & ~fetch
        with m.If(done):
            m.d.sync += [active.eq(0), out_valid.eq(0)]
            for i, p in enumerate(self.lanes):
                m.d.comb += p.release.eq(cur == i)

        idle = ~active & ~done & self.enable
        start = Signal()
        m.d.comb += start.eq(idle & Cat(*(p.ready for p in self.lanes)).any())
        with m.If(idle):
            # Next lane with a packet, after the current one.
            for k in reversed(range(1, n + 1)):
                lane = Signal(range(n), name=f'try{k}')
                m.d.comb += lane.eq((cur + k) % n if n > 1 else 0)
                with m.If(ready[lane]):
                    m.d.sync += [cur.eq(lane), active.eq(1), addr.eq(0)]

        # Bursts of the started packet not yet addressed. The DMA sends no data before its
        # address, so they are all addressed by the time the packet's last word goes.
        bursts = Signal(range(BURSTS_PER_PACKET + 1))
        with m.If(start):
            m.d.sync += bursts.eq(BURSTS_PER_PACKET)
        with m.Elif(self.aw_accepted):
            m.d.sync += bursts.eq(bursts - 1)
        m.d.comb += self.dma_enable.eq(bursts != 0)
        return m
