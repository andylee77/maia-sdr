// One live audio stream's jitter buffer: an 8 kHz ring resampled to the output rate by a slow
// PLL on ring fill (target 150 ms), through a windowed-sinc interpolator (32 taps, cut off at
// 3.76 kHz) so the voice band leaves no images above 4 kHz. An empty ring re-primes at once: the
// next transmission (often < 1 s after the last) waits for 150 ms of audio instead of playing each
// 20 ms frame as it lands.
//
// Each sample keeps the pan of its talkgroup (-1 left, 0 both, +1 right), so a talkgroup change
// inside the buffer switches speaker exactly. One ring per lane, mixed into the output at the
// lane's own volume (`vol`). It holds 8 s: a call held back until its alert tone (gate.js) plays
// from its start. The class
// is shared by the AudioWorklet (its source text is injected into the worklet, see sources.js, so
// it may use no imports or globals) and the ScriptProcessor fallback. `now` is in seconds.
//
// Volume normalization (`norm`): the server's AGC starts a transmission at its radio's recent level
// (else at unity) and settles over about a second; the leveler evens out what is left. It measures
// each transmission from its first 20 ms of voice (a silence of 1.5 s starts a new one), follows
// loud peaks fast and quiet passages slowly, and holds its gain through silence (no noise pumping).
// Every stream then passes a soft knee above 0.8 (-2 dBFS) after the interpolator, so neither a
// boosted peak nor the waveform between samples, which can rise past the samples themselves,
// reaches full scale.

export class Ring {
  constructor(base) {
    this.N = 65536;
    this.buf = new Float32Array(this.N);
    this.pan = new Int8Array(this.N);
    this.w = 0; this.r = 0; this.avail = 0; this.frac = 0;
    this.base = base; this.ratio = base;
    this.TARGET = 1200; this.KP = 4e-5; this.MAXDEV = 0.005;
    this.priming = true; this.underruns = 0; this.dryAt = -1;
    this.norm = false; this.lvl = 0; this.gain = 1; this.lastVoice = -10;
    this.vol = 1;
    this.TAPS = 32; this.PHASES = 128;
    this.taps = Ring.interpolator(this.TAPS, this.PHASES);
  }

  // The interpolator's taps for PHASES + 1 positions between two samples (row p at p / PHASES),
  // each over samples r - TAPS/2 + 1 .. r + TAPS/2: a Kaiser-windowed sinc (beta 7) cut off at
  // 0.47 of the 8 kHz rate, every row summing to one.
  static interpolator(taps, phases) {
    const half = taps / 2, fc = 0.47, beta = 7;
    const i0 = x => {
      let s = 1, t = 1;
      for (let k = 1; k < 25; k++) { t *= (x / (2 * k)) ** 2; s += t; }
      return s;
    };
    const rows = new Float32Array((phases + 1) * taps);
    for (let p = 0; p <= phases; p++) {
      const row = rows.subarray(p * taps, (p + 1) * taps);
      let sum = 0;
      for (let j = 0; j < taps; j++) {
        const t = j - half + 1 - p / phases, a = 2 * Math.PI * fc * t;
        const w = i0(beta * Math.sqrt(Math.max(0, 1 - (t / half) ** 2))) / i0(beta);
        row[j] = 2 * fc * (a === 0 ? 1 : Math.sin(a) / a) * w;
        sum += row[j];
      }
      for (let j = 0; j < taps; j++) row[j] /= sum;
    }
    return rows;
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
      // Gain ramps across the block (no zipper noise).
      if (this.norm) v *= g0 + (g1 - g0) * (i / d.length);
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
    const { N, TAPS, PHASES, buf, taps } = this;
    for (let i = 0; i < L.length; i++) {
      if (this.avail <= 1) { this.dryAt = now; this.priming = true; return; }
      // Samples past the newest one count as zero.
      const n = Math.min(TAPS, this.avail + TAPS / 2 - 1);
      const q = this.frac * PHASES, ph = q | 0, mu = q - ph;
      let s0 = 0, s1 = 0;
      for (let j = 0, k = (this.r - TAPS / 2 + 1 + N) % N, t0 = ph * TAPS; j < n; j++, k = (k + 1) % N) {
        s0 += buf[k] * taps[t0 + j];
        s1 += buf[k] * taps[t0 + TAPS + j];
      }
      // The soft knee above 0.8 (see the header).
      const s = s0 + mu * (s1 - s0), a = Math.abs(s);
      const v = (a > 0.8 ? Math.sign(s) * (0.8 + 0.2 * Math.tanh((a - 0.8) / 0.2)) : s) * this.vol;
      const p = this.pan[this.r];
      if (R) { if (p <= 0) L[i] += v; if (p >= 0) R[i] += v; } else L[i] += v;
      this.frac += this.ratio;
      while (this.frac >= 1) {
        this.frac -= 1; this.r = (this.r + 1) % this.N; this.avail--;
        if (this.avail <= 0) break;
      }
    }
  }
}

// Two lanes at once can sum past full scale.
export function clip(buf) {
  for (let i = 0; i < buf.length; i++) {
    const v = buf[i];
    if (v > 1) buf[i] = 1; else if (v < -1) buf[i] = -1;
  }
}
