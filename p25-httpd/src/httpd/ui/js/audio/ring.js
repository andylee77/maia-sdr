// One live audio stream's jitter buffer: an 8 kHz ring resampled to the
// output rate by a slow PLL on ring fill (target 150 ms). An empty ring
// re-primes at once: the next transmission (often < 1 s after the last)
// waits for 150 ms of audio instead of playing each 20 ms frame as it
// lands (bench 2026-09-27).
//
// Change 062: each sample keeps the pan of its talkgroup (-1 left, 0
// both, +1 right), so a talkgroup change inside the buffer switches
// speaker exactly.
//
// Change 066: one ring per traffic chain, mixed into the output. The
// class is shared by the AudioWorklet (its source text is injected into
// the worklet, see sources.js, so it may use no imports or globals) and
// the ScriptProcessor fallback. `now` is in seconds.
//
// Change 067: volume normalization (`norm`). The vocoder's AGC restarts
// at unity gain on every call and takes about a second to settle, so a
// quiet or loud talker is heard at their own level first. The leveler
// measures each transmission from its first 20 ms of voice (a silence of
// 1.5 s starts a new one), follows loud peaks fast and quiet passages
// slowly, holds its gain through silence (no noise pumping), and a soft
// limiter above -2 dBFS keeps boosted peaks from clipping.

export class Ring {
  constructor(base) {
    this.N = 16384;
    this.buf = new Float32Array(this.N);
    this.pan = new Int8Array(this.N);
    this.w = 0; this.r = 0; this.avail = 0; this.frac = 0;
    this.base = base; this.ratio = base;
    this.TARGET = 1200; this.KP = 4e-5; this.MAXDEV = 0.005;
    this.priming = true; this.underruns = 0; this.dryAt = -1;
    this.norm = false; this.lvl = 0; this.gain = 1; this.lastVoice = -10;
  }

  // Gain for the next block from its RMS (see the header).
  level(rms, now) {
    const TARGET = 0.1, GATE = 0.004;
    if (rms < GATE) return this.gain;
    if (this.lvl === 0 || now - this.lastVoice > 1.5) this.lvl = rms;
    else this.lvl += (rms > this.lvl ? 0.25 : 0.04) * (rms - this.lvl);
    this.lastVoice = now;
    const want = Math.max(0.25, Math.min(8, TARGET / this.lvl));
    return this.gain + 0.3 * (want - this.gain);
  }

  // `d`: samples in [-1, 1] (or Int16 with `scale` 1/32768).
  write(d, pan, now, scale = 1) {
    // An underrun is a dry ring that audio refills within 300 ms (a gap
    // inside a transmission), not the silence after one ends.
    if (this.dryAt >= 0 && now - this.dryAt < 0.3) this.underruns++;
    this.dryAt = -1;
    let g0 = 1, g1 = 1;
    if (this.norm) {
      let sq = 0;
      for (let i = 0; i < d.length; i++) sq += d[i] * d[i];
      const rms = Math.sqrt(sq / Math.max(1, d.length)) * scale;
      g0 = this.gain;
      g1 = this.gain = this.level(rms, now);
    }
    for (let i = 0; i < d.length; i++) {
      let v = d[i] * scale;
      if (this.norm) {
        // Gain ramps across the block (no zipper noise); soft knee at 0.8.
        v *= g0 + (g1 - g0) * (i / d.length);
        const a = Math.abs(v);
        if (a > 0.8) v = Math.sign(v) * (0.8 + 0.2 * Math.tanh((a - 0.8) / 0.2));
      }
      this.buf[this.w] = v;
      this.pan[this.w] = pan;
      this.w = (this.w + 1) % this.N;
      if (this.avail < this.N) this.avail++; else this.r = (this.r + 1) % this.N;
    }
    if (this.priming && this.avail >= this.TARGET) this.priming = false;
  }

  // Add this stream to L (and R when stereo) for L.length output samples.
  mix(L, R, now) {
    if (this.priming) return;
    let dev = this.KP * (this.avail - this.TARGET);
    dev = Math.max(-this.MAXDEV, Math.min(this.MAXDEV, dev));
    this.ratio = this.base * (1 + dev);
    for (let i = 0; i < L.length; i++) {
      if (this.avail <= 1) { this.dryAt = now; this.priming = true; return; }
      const a = this.buf[this.r], b = this.buf[(this.r + 1) % this.N];
      const v = a + (b - a) * this.frac, p = this.pan[this.r];
      if (R) { if (p <= 0) L[i] += v; if (p >= 0) R[i] += v; } else L[i] += v;
      this.frac += this.ratio;
      while (this.frac >= 1) {
        this.frac -= 1; this.r = (this.r + 1) % this.N; this.avail--;
        if (this.avail <= 0) break;
      }
    }
  }
}

// Two chains at once can sum past full scale.
export function clip(buf) {
  for (let i = 0; i < buf.length; i++) {
    const v = buf[i];
    if (v > 1) buf[i] = 1; else if (v < -1) buf[i] = -1;
  }
}
