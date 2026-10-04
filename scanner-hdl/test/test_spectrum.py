#
# Fishball radio core - the framed spectrometer (radio_core/spectrum.py, frame_integrator.py;
# doc/changes/082 items 4-6).
#
# Random samples go into SpectrumFrames; the AXI write model (strict) takes the frames the DMA
# writes. The FFT's output is recorded as the integrator sees it, and checked against the FFT's
# own model on the input transforms (which also fixes which input sample began each transform).
# A Python reference of the integrator's frame boundaries (integrations, mode, alternate, abort,
# all taken when a frame starts) splits the recorded transforms into frames; each frame the DMA
# wrote must equal, word for word, the integrator's model on those transforms with the header
# (spec.header_words) in bins 0-7 and the check in the last bin.
#
# SPDX-License-Identifier: MIT
#

import os
import unittest

from amaranth import *
from amaranth.sim import Simulator
import numpy as np

from radio_core.common_edge import CommonEdge
from radio_core.spectrum import (
    FLAG_ABORTED, FLAG_PEAK, HEADER_WORDS, MAGIC, SpectrumFrames, bin_word, fold, frame_words,
    header_words)

from .hwval_axi_wmodel import AxiWriteSlaveModel

SLOW = os.environ.get('MAIA_HDL_SLOW_TESTS') == '1'


class Harness(Elaboratable):
    """The spectrometer with each block's common edge, as the core wires it."""
    def __init__(self, order_log2):
        self.spec = SpectrumFrames(0x2100_0000, 2, dma_name='m_axi_spec', order_log2=order_log2)

    def elaborate(self, platform):
        m = Module()
        m.domains += [ClockDomain('clk2x'), ClockDomain('clk3x')]
        m.submodules.spec = spec = self.spec
        m.submodules.ce2x = ce2x = CommonEdge('sync', 'clk2x', 2, 'ce2x')
        m.submodules.ce_int = ce_int = CommonEdge('sync', 'clk3x', 3, 'ce_int')
        for j, edge in enumerate(spec.common_edge_twiddles):
            gen = CommonEdge('sync', 'clk3x', 3, f'ce_twiddle{j}')
            m.submodules[f'ce_twiddle{j}'] = gen
            m.d.comb += edge.eq(gen.common_edge)
        m.d.comb += [
            spec.common_edge_2x.eq(ce2x.common_edge),
            spec.common_edge_integrator.eq(ce_int.common_edge),
        ]
        return m


def sample_index_of(n):
    """The sample index the bench gives the n-th sample into the spectrometer (not n itself, so
    that the header shows which sample's index was taken)."""
    return 10_000 + 3 * n


def clips_of(n):
    return 7 + n // 5


class Reference:
    """The integrator's frame boundaries: integrations, mode and alternate are taken when a frame
    starts; an abort ends the frame at the end of the transform it falls in."""
    def __init__(self):
        self.sum_counter = 0
        self.do_abort = False
        self.peak = False
        self.current = []
        self.frames = []

    def transform(self, t, nint, peak, alternate, abort):
        self.do_abort |= abort
        self.current.append(t)
        natural = self.sum_counter in (0, 1)
        if natural or self.do_abort:
            self.frames.append(dict(transforms=self.current, peak=self.peak,
                                    aborted=self.do_abort and not natural))
            self.sum_counter = nint
            self.peak = (not self.peak) if alternate else peak
            self.do_abort = False
            self.current = []
        else:
            self.sum_counter -= 1


class TestSpectrumFrames(unittest.TestCase):
    def run_frames(self, order_log2, schedule, n_transforms, *, seed=1, wready_prob=0.7):
        """Run `n_transforms` input transforms (strobes on two cycles of three). `schedule` maps
        an input sample number to the controls set from then on (`nint`, `peak`, `alternate`) or
        an abort pulse (`abort`)."""
        n = 1 << order_log2
        h = Harness(order_log2)
        spec = h.spec
        fft = spec.fft
        model = AxiWriteSlaveModel(spec.dma.axi, strict=True, seed=seed, awready_prob=0.8,
                                   wready_prob=wready_prob, b_latency=(1, 6))
        rng = np.random.default_rng(seed)
        total = n_transforms * n + fft.delay + 2 * n
        re_in = rng.integers(-2048, 2048, total)
        im_in = rng.integers(-2048, 2048, total)
        rec = dict(re=[], im=[], ends=[], ctrl=[], aborts=[])

        async def bench(ctx):
            ctrl = dict(nint=1, peak=0, alternate=0)
            abort_pending = False
            k = 0          # samples given
            cycle = 0
            while k < total:
                strobe = cycle % 3 != 2
                cycle += 1
                if strobe:
                    todo = schedule.get(k, {})
                    for key, value in todo.items():
                        if key != 'abort':
                            ctrl[key] = value
                    ctx.set(spec.number_integrations, ctrl['nint'])
                    ctx.set(spec.peak_detect, ctrl['peak'])
                    ctx.set(spec.alternate, ctrl['alternate'])
                    ctx.set(spec.re_in, int(re_in[k]))
                    ctx.set(spec.im_in, int(im_in[k]))
                    ctx.set(spec.sample_index, sample_index_of(k))
                    ctx.set(spec.adc_clips, clips_of(k))
                    ctx.set(spec.strobe_in, 1)
                    # What the integrator takes at this edge.
                    rec['re'].append(ctx.get(fft.re_out))
                    rec['im'].append(ctx.get(fft.im_out))
                    if ctx.get(fft.out_last):
                        rec['ends'].append(len(rec['re']) - 1)
                        rec['ctrl'].append(dict(ctrl))
                        rec['aborts'].append(abort_pending)
                        abort_pending = False
                    # An abort at a transform's last sample ends the transform after it.
                    if 'abort' in todo:
                        ctx.set(spec.abort, 1)
                        abort_pending = True
                    k += 1
                else:
                    ctx.set(spec.strobe_in, 0)
                await ctx.tick()
                ctx.set(spec.abort, 0)
            ctx.set(spec.strobe_in, 0)
            await ctx.tick().repeat(3 * n + 400)

        sim = Simulator(h)
        sim.add_clock(12e-9, domain='sync')
        sim.add_clock(4e-9, domain='clk3x', phase=6e-9)
        sim.add_clock(6e-9, domain='clk2x', phase=6e-9)
        sim.add_testbench(model.bench, background=True)
        sim.add_testbench(bench)
        sim.run()
        return spec, re_in, im_in, rec, model

    def check(self, order_log2, schedule, n_transforms, **kw):
        n = 1 << order_log2
        spec, re_in, im_in, rec, model = self.run_frames(order_log2, schedule, n_transforms, **kw)
        fft, integrator = spec.fft, spec.integrator
        re_out = np.array(rec['re'])
        im_out = np.array(rec['im'])

        # The FFT's output: input transform m (samples m n .. m n + n - 1) ends at the sample
        # numbered `delay + m n + n - 1`.
        ends = rec['ends']
        first_m = None
        for j, end in enumerate(ends):
            m = (end - fft.delay - (n - 1)) // n
            self.assertEqual(end, fft.delay + m * n + n - 1)
            if m < 0:
                continue
            if first_m is None:
                first_m = j
            want_re, want_im = fft.model(re_in[m * n:(m + 1) * n], im_in[m * n:(m + 1) * n])
            np.testing.assert_equal(re_out[end - n + 1:end + 1], want_re, f'transform {m}')
            np.testing.assert_equal(im_out[end - n + 1:end + 1], want_im, f'transform {m}')

        ref = Reference()
        for j, end in enumerate(ends):
            c = rec['ctrl'][j]
            ref.transform(j, c['nint'], c['peak'], c['alternate'], rec['aborts'][j])

        stream = model.stream()
        written = [stream[i * n:(i + 1) * n] for i in range(len(stream) // n)]
        self.assertGreaterEqual(len(written), 4)
        checked = []
        for seq, (frame, words) in enumerate(zip(ref.frames, written)):
            # Every frame's check: its 32-bit halves XOR to zero.
            x = 0
            for w in words:
                x ^= fold(w)
            self.assertEqual(x, 0, f'frame {seq}: check')
            self.assertEqual(words[0] & 0xFFFF, MAGIC)
            self.assertEqual((words[2] & 0xFFFF_FFFF), seq)
            ts = frame['transforms']
            if ts[0] < first_m:
                continue           # holds the FFT's start-up output
            last_end = ends[ts[-1]]
            m0 = (ends[ts[0]] - fft.delay - (n - 1)) // n
            sel = slice(last_end - len(ts) * n + 1, last_end + 1)
            value, exponent = integrator.model(len(ts), re_out[sel], im_out[sel], frame['peak'])
            bins = [bin_word(int(v), int(e)) for v, e in zip(value, exponent)]
            header = header_words(peak=frame['peak'], aborted=frame['aborted'],
                                  transforms=len(ts), sample_index=sample_index_of(m0 * n),
                                  sequence=seq, adc_clips=clips_of(m0 * n))
            want = frame_words(header, bins)
            for i, (got, exp) in enumerate(zip(words, want)):
                self.assertEqual(got, exp, f'frame {seq} word {i}: {got:#x} != {exp:#x}')
            checked.append(frame)
        return ref.frames, checked

    def test_modes_and_integrations_per_frame(self):
        """Average frames of 3, one ended early by an abort in its first transform; peak from
        the next frame after a mid-frame write; then alternate; then 2 integrations from the next
        frame. Every frame equals the model, and the header records each change from the frame it
        applies to."""
        n = 256
        schedule = {
            0: dict(nint=3),
            6 * n + 120: dict(abort=1),
            9 * n + 70: dict(peak=1),
            17 * n + 100: dict(alternate=1),
            27 * n + 30: dict(nint=2),
        }
        frames, checked = self.check(8, schedule, 44)
        self.assertGreaterEqual(len(checked), 12)
        modes = [f['peak'] for f in checked]
        self.assertIn(True, modes)
        self.assertIn(False, modes)
        alternating = [f['peak'] for f in checked if len(f['transforms']) == 2]
        self.assertTrue(all(a != b for a, b in zip(alternating, alternating[1:])))
        self.assertTrue(any(f['aborted'] for f in checked))
        self.assertEqual({len(f['transforms']) for f in checked if not f['aborted']}, {2, 3})

    def test_header_flags(self):
        self.assertEqual(header_words(peak=True, aborted=True, transforms=5, sample_index=9,
                                      sequence=2, adc_clips=3)[0],
                         MAGIC | 1 << 16 | (FLAG_PEAK | FLAG_ABORTED) << 24 | 5 << 32)
        self.assertEqual(HEADER_WORDS, 8)

    @unittest.skipUnless(SLOW, 'the 4096-point spectrometer takes minutes (MAIA_HDL_SLOW_TESTS=1)')
    def test_the_core_s_spectrometer(self):
        """The core's 4096-point instance: frames of one transform, alternating."""
        schedule = {0: dict(nint=1, alternate=1)}
        frames, checked = self.check(12, schedule, 7, wready_prob=0.9)
        self.assertGreaterEqual(len(checked), 4)


if __name__ == '__main__':
    unittest.main()
