#
# Fishball P25 - lane packets and the lane ring (doc/changes/079, step 3a)
#
# Three LanePacketizers, the LaneRing and the production DmaStreamRingWrite write into
# AxiWriteSlaveModel, which checks the AXI rules on every edge. The test drives each lane's
# samples, tag, NCO and enable, and the shared sample index (the cycle number) and clip count,
# then parses the written stream into packets and checks every one against the Python reference
# (lane_packetizer.header_words), the samples it should hold and the lane's packet before it.
#
# SPDX-License-Identifier: MIT
#

import unittest

from amaranth import *
from amaranth.sim import Simulator

from maia_hdl.dma import DmaStreamRingWrite
from p25_hdl.lane_packetizer import (
    FLAG_LAST, FLAG_LOST, FLAG_RETUNED, HEADER_WORDS, MAGIC, MAX_SAMPLES, PACKET_WORDS,
    LanePacketizer, header_words, iq_word)
from p25_hdl.lane_ring import BURSTS_PER_PACKET, LaneRing

from .hwval_axi_wmodel import AxiWriteSlaveModel

LANES = 3
BASE = 0x1900_0000


def sample(lane, index):
    """The sample a lane makes at a sample index: covers the full 16-bit range."""
    re = (index * 7919 + lane * 1237) & 0xFFFF
    im = (index * 104729 + lane * 4111 + 5) & 0xFFFF
    return (re - 0x10000 if re & 0x8000 else re, im - 0x10000 if im & 0x8000 else im)


def clips_at(cycle):
    return (cycle // 3) & 0xFFFF_FFFF


class LaneRingHarness(Elaboratable):
    def __init__(self):
        self.packetizers = [LanePacketizer(i) for i in range(LANES)]
        self.ring = LaneRing(self.packetizers)
        # 4 sub-buffers of 16 KB: 16 packets before the ring wraps.
        self.dma = DmaStreamRingWrite(BASE, 2, 0x4000, width=64, axi_awidth=32,
                                      name='m_axi_lanes')
        self.sample_index = Signal(64)
        self.adc_clips = Signal(32)

    def elaborate(self, platform):
        m = Module()
        for i, p in enumerate(self.packetizers):
            m.submodules[f'lane{i}'] = p
            m.d.comb += [p.sample_index.eq(self.sample_index), p.adc_clips.eq(self.adc_clips)]
        m.submodules.ring = self.ring
        m.submodules.dma = self.dma
        m.d.comb += [
            self.dma.stream_data.eq(self.ring.stream_data),
            self.dma.stream_valid.eq(self.ring.stream_valid),
            self.ring.stream_ready.eq(self.dma.stream_ready),
            self.dma.enable.eq(self.ring.dma_enable),
            self.ring.aw_accepted.eq(self.dma.axi.aw_handshake()),
        ]
        return m


class Lane:
    """One lane's plan: what it does on which cycle, and what it sent."""
    def __init__(self, lane, gap, start):
        self.lane = lane
        self.gap = gap
        self.cycle = start
        self.events = {}     # cycle -> list of ('sample', tag, nco) / ('enable', v)
        self.sent = []       # (cycle, tag, nco, run) of every sample made while enabled
        self.run = 0

    def samples(self, n, tag, nco):
        for _ in range(n):
            self.events.setdefault(self.cycle, []).append(('sample', tag, nco))
            self.sent.append((self.cycle, tag, nco, self.run))
            self.cycle += self.gap
        return self

    def disable(self, cycles):
        self.events.setdefault(self.cycle, []).append(('enable', 0))
        self.cycle += cycles
        self.events.setdefault(self.cycle, []).append(('enable', 1))
        self.cycle += self.gap
        self.run += 1
        return self

    def expected(self):
        """The packets the lane makes when nothing is lost (header fields, samples)."""
        packets = []
        cur = None
        last_tag = None
        for cycle, tag, nco, run in self.sent:
            if cur is not None and (tag != cur['tag'] or run != cur['run']):
                if run != cur['run']:
                    cur['flags'] |= FLAG_LAST
                packets.append(cur)
                last_tag = cur['tag']
                cur = None
            if cur is None:
                first_of_run = not packets or packets[-1]['run'] != run
                cur = dict(tag=tag, nco=nco, run=run, index=cycle, clips=clips_at(cycle),
                           samples=[], flags=FLAG_RETUNED if first_of_run or tag != last_tag
                           else 0)
            cur['samples'].append(sample(self.lane, cycle))
            if len(cur['samples']) == MAX_SAMPLES:
                packets.append(cur)
                last_tag = cur['tag']
                cur = None
        if cur is not None and cur['run'] != self.run:
            # Closed by the plan's last disable.
            cur['flags'] |= FLAG_LAST
            packets.append(cur)
            cur = None
        # A packet still filling at the end never reaches the ring.
        return packets


def payload_words(samples):
    words = []
    for k in range(0, len(samples), 2):
        pair = samples[k:k + 2]
        words.append(iq_word(*pair[0], *(pair[1] if len(pair) > 1 else (0, 0))))
    return words


def parse(stream):
    """Split the written stream into packets: (lane, header words, payload words)."""
    assert len(stream) % PACKET_WORDS == 0, len(stream)
    packets = []
    for k in range(0, len(stream), PACKET_WORDS):
        words = stream[k:k + PACKET_WORDS]
        packets.append(((words[0] >> 20) & 0xF, words[:HEADER_WORDS], words[HEADER_WORDS:]))
    return packets


def fields(w0):
    return dict(magic=w0 & 0xFFFF, format=(w0 >> 16) & 0xF, lane=(w0 >> 20) & 0xF,
                flags=(w0 >> 24) & 0xFF, count=(w0 >> 32) & 0xFFFF, tag=w0 >> 48)


class LaneRingTest(unittest.TestCase):
    def make(self, **model_kw):
        self.h = LaneRingHarness()
        self.model = AxiWriteSlaveModel(self.h.dma.axi, **model_kw)
        self.lost = [0] * LANES
        self.long_waits = []

    def run_plan(self, lanes, *, ring_enable=lambda cycle: True, drain=4000):
        h = self.h
        end = max(lane.cycle for lane in lanes) + drain

        async def bench(ctx):
            for p in h.packetizers:
                ctx.set(p.enable, 1)
            for cycle in range(end):
                ctx.set(h.sample_index, cycle)
                ctx.set(h.adc_clips, clips_at(cycle))
                ctx.set(h.ring.enable, int(ring_enable(cycle)))
                for lane in lanes:
                    p = h.packetizers[lane.lane]
                    strobe = 0
                    for ev in lane.events.get(cycle, []):
                        if ev[0] == 'sample':
                            _, tag, nco = ev
                            re, im = sample(lane.lane, cycle)
                            ctx.set(p.re_in, re)
                            ctx.set(p.im_in, im)
                            ctx.set(p.tag, tag)
                            ctx.set(p.nco, nco)
                            strobe = 1
                        else:
                            ctx.set(p.enable, ev[1])
                    ctx.set(p.strobe_in, strobe)
                for i, p in enumerate(h.packetizers):
                    self.lost[i] += ctx.get(p.lost)
                await ctx.tick()

        sim = Simulator(h)
        sim.add_clock(16e-9)
        sim.add_testbench(self.model.bench, background=True)
        sim.add_testbench(bench)
        sim.run()
        return parse(self.model.stream())

    def check_packet(self, lane, header, payload):
        """Header fields, check word, samples and padding of one packet, against the samples
        the lane made from the packet's sample index on."""
        f = fields(header[0])
        self.assertEqual((f['magic'], f['format'], f['lane']), (MAGIC, 1, lane))
        self.assertTrue(1 <= f['count'] <= MAX_SAMPLES, f)
        index = header[1]
        samples = [sample(lane, index + k * self.gaps[lane]) for k in range(f['count'])]
        words = payload_words(samples)
        self.assertEqual(payload[:len(words)], words)
        self.assertEqual(payload[len(words):], [0] * (len(payload) - len(words)))
        power = sum(re * re + im * im for re, im in samples)
        peak = max(max(abs(re), abs(im)) for re, im in samples)
        expect = header_words(
            lane=lane, flags=f['flags'], count=f['count'], tag=f['tag'], sample_index=index,
            power=power, peak=peak, nco=header[3] & 0xFFF_FFFF, sequence=(header[3] >> 32) & 0xFFFF,
            adc_clips=clips_at(index), payload=payload)
        self.assertEqual(header, expect)
        return f

    def by_lane(self, packets):
        lanes = {i: [] for i in range(LANES)}
        for lane, header, payload in packets:
            lanes[lane].append((header, payload))
        return lanes

    def check_axi(self, packets):
        # Every address the DMA issued has its data: no AW waits for a stream.
        self.assertEqual(self.model.aw_count, len(self.model.bursts))
        self.assertEqual(self.model.open_w_beats(), 0)
        self.assertEqual(len(self.model.bursts), len(packets) * BURSTS_PER_PACKET)
        waits = [b['wlast_cycle'] - b['aw_cycle'] for b in self.model.bursts]
        return max(waits)

    def test_full_packets_tag_changes_and_disable(self):
        self.make()
        self.gaps = [4, 5, 7]
        lanes = [
            # The pause lets the ring drain the lane's slots (a disable closes a packet early).
            Lane(0, 4, 3).samples(2 * MAX_SAMPLES + 37, 0x0100, 0x111_1111).disable(1500)
                         .samples(10, 0x0100, 0x111_1111).disable(10),
            # A tag change at an odd count, then back to the first tag.
            Lane(1, 5, 0).samples(301, 0xBEEF, 0x123_4567).samples(400, 0xCAFE, 0x765_4321)
                         .samples(MAX_SAMPLES + 1, 0xBEEF, 0x123_4567).disable(10),
            Lane(2, 7, 1).samples(MAX_SAMPLES, 0x0002, 0xFFF_FFFF).disable(30)
                         .samples(5, 0x0003, 0x000_0001).disable(10),
        ]
        packets = self.run_plan(lanes)
        self.assertEqual(sum(self.lost), 0)
        got = self.by_lane(packets)
        for lane in lanes:
            expected = lane.expected()
            with self.subTest(lane=lane.lane):
                self.assertEqual(len(got[lane.lane]), len(expected))
                for seq, ((header, payload), exp) in enumerate(zip(got[lane.lane], expected)):
                    f = self.check_packet(lane.lane, header, payload)
                    self.assertEqual(
                        (f['count'], f['tag'], f['flags'], header[1], header[3] & 0xFFF_FFFF,
                         (header[3] >> 32) & 0xFFFF),
                        (len(exp['samples']), exp['tag'], exp['flags'], exp['index'], exp['nco'],
                         seq), f'packet {seq}')
        # Lane 0: two full, 37 closed by a disable, ten closed by the next. Lane 1: 301 and 400
        # closed by tag changes, a full one, one sample closed by the disable. Lane 2: a full one
        # (the disable after it closes nothing), five closed by the disable.
        self.assertEqual([len(got[i]) for i in range(LANES)], [4, 4, 2])
        # Addresses are issued only for packets in hand: an AW waits for at most a burst or two.
        self.assertLess(self.check_axi(packets), 64)

    def test_lost_samples_are_flagged(self):
        """With the ring stopped a lane fills both slots and drops samples; the packet after
        the gap says so, and its sample index shows how many."""
        self.make()
        self.gaps = [4, 4, 4]
        # Both slots are full after 2016 samples (cycle 8064); the ring starts at 10080.
        lanes = [Lane(0, 4, 0).samples(5 * MAX_SAMPLES, 0x0042, 0x0AB_CDEF)]
        packets = self.run_plan(lanes, ring_enable=lambda cycle: cycle >= 10080)
        got = self.by_lane(packets)[0]
        self.assertGreater(self.lost[0], 500)
        self.assertEqual(len(got), 4)
        dropped = 0
        previous = None
        flags = []
        for seq, (header, payload) in enumerate(got):
            f = self.check_packet(0, header, payload)
            flags.append(f['flags'])
            self.assertEqual((header[3] >> 32) & 0xFFFF, seq)
            if previous is not None:
                gap = header[1] - (previous[1] + 4 * fields(previous[0])['count'])
                self.assertEqual(bool(f['flags'] & FLAG_LOST), gap > 0)
                dropped += gap // 4
            previous = header
        self.assertEqual(flags, [FLAG_RETUNED, 0, FLAG_LOST, 0])
        self.assertEqual(dropped, self.lost[0])
        self.check_axi(packets)

    def test_backpressure_and_ring_disable_between_packets(self):
        """A slow, stalling interconnect, and the ring switched off and on while packets are
        in flight: every packet is whole and in order per lane, every AW has its data."""
        self.make(seed=7, awready_prob=0.4, wready_prob=0.5, b_latency=(2, 30))
        self.gaps = [6, 6, 6]
        lanes = [Lane(i, 6, i).samples(3 * MAX_SAMPLES, 0x10 + i, 0x100 * (i + 1))
                 for i in range(LANES)]
        packets = self.run_plan(
            lanes, ring_enable=lambda cycle: cycle % 3000 not in range(1000, 1300), drain=8000)
        self.assertEqual(sum(self.lost), 0)
        got = self.by_lane(packets)
        for i in range(LANES):
            self.assertEqual(len(got[i]), 3)
            for seq, (header, payload) in enumerate(got[i]):
                self.check_packet(i, header, payload)
                self.assertEqual((header[3] >> 32) & 0xFFFF, seq)
        self.check_axi(packets)


if __name__ == '__main__':
    unittest.main()
