// The TV sound's jitter buffer: two channels at the stream's rate, resampled to the output's by a
// slow PLL on its fill (target 0.5 s), as `Ring` (../audio/ring.js) does for the radio's voice.
// It keeps each syncframe's presentation time, so the time of the sample playing now is known.
// The class is shared by the AudioWorklet (its source text is injected into the worklet, see
// sources.js, so it may use no imports or globals) and the ScriptProcessor fallback.

export class StereoRing {
  constructor(inRate, outRate) {
    this.N = 1 << 17;
    this.L = new Float32Array(this.N);
    this.R = new Float32Array(this.N);
    this.w = 0; this.r = 0; this.avail = 0; this.frac = 0;
    this.inRate = inRate;
    this.base = inRate / outRate; this.ratio = this.base;
    this.TARGET = inRate / 2; this.KP = 1e-7; this.MAXDEV = 0.002;
    this.priming = true; this.underruns = 0; this.started = false;
    // Samples written and read since the start, and [written, pts] where each timed block began.
    this.written = 0; this.read = 0; this.stamps = [];
  }

  // `d`: samples interleaved left, right; `pts`: its first sample's time (90 kHz), or -1.
  write(d, pts) {
    if (pts >= 0) this.stamps.push([this.written, pts]);
    for (let i = 0; i + 1 < d.length; i += 2) {
      // Full (the page stalled): the oldest goes.
      if (this.avail === this.N) { this.r = (this.r + 1) % this.N; this.avail--; this.read++; }
      this.L[this.w] = d[i];
      this.R[this.w] = d[i + 1];
      this.w = (this.w + 1) % this.N;
      this.avail++; this.written++;
    }
    if (this.priming && this.avail >= this.TARGET) { this.priming = false; this.started = true; }
  }

  // Add the next L.length output samples to L and R.
  mix(L, R) {
    if (this.priming) return;
    let dev = this.KP * (this.avail - this.TARGET);
    dev = Math.max(-this.MAXDEV, Math.min(this.MAXDEV, dev));
    this.ratio = this.base * (1 + dev);
    for (let i = 0; i < L.length; i++) {
      if (this.avail <= 1) { this.underruns++; this.priming = true; return; }
      const a = this.r, b = (this.r + 1) % this.N;
      L[i] += this.L[a] + (this.L[b] - this.L[a]) * this.frac;
      R[i] += this.R[a] + (this.R[b] - this.R[a]) * this.frac;
      this.frac += this.ratio;
      while (this.frac >= 1 && this.avail > 1) {
        this.frac -= 1; this.r = (this.r + 1) % this.N; this.avail--; this.read++;
      }
    }
  }

  // The time (90 kHz) of the sample playing now, or -1 before the first timed block.
  pts() {
    while (this.stamps.length > 1 && this.stamps[1][0] <= this.read) this.stamps.shift();
    const s = this.stamps[0];
    if (!s || s[0] > this.read) return -1;
    return (s[1] + Math.round((this.read - s[0]) * 90000 / this.inRate)) % 8589934592;
  }
}
