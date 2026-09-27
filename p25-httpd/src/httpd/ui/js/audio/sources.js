// Source text of the AudioWorklet processor and the /ws/audio Worker,
// loaded through Blob URLs (no extra routes). Same design as the
// pre-056 dashboard (proven on the board):
//
//   net -> Worker (own thread, WebSocket) -> MessagePort -> AudioWorklet
//
// The worklet keeps an 8 kHz ring and resamples continuously to the
// context rate with a slow PLL on ring fill (target 150 ms). An empty
// ring re-primes at once: the next transmission (often < 1 s after the
// last) waits for 150 ms of audio instead of playing each 20 ms frame
// as it lands, which the old 2 s rule made choppy (bench 2026-09-27).
//
// Change 062: speaker routing. The server names the talkgroup in a
// {"type":"meta"} text frame before its audio; the Worker maps it to a
// pan (-1 left, 0 both, +1 right, see player.js `routeFromPrefs`) and
// tags each block, and the worklet keeps the pan per sample so a
// talkgroup change inside the 150 ms buffer switches speaker exactly.

export const WORKLET_SRC = `
class P25Audio extends AudioWorkletProcessor {
  constructor() {
    super();
    this.RING = 16384; this.ring = new Float32Array(this.RING); this.pan = new Int8Array(this.RING);
    this.w = 0; this.r = 0; this.avail = 0; this.frac = 0;
    this.base = 8000 / sampleRate; this.ratio = this.base;
    this.TARGET = 1200; this.KP = 4e-5; this.MAXDEV = 0.005;
    this.priming = true; this.underruns = 0; this.dryAt = -1;
    this.port.onmessage = ev => {
      const m = ev.data;
      if (m && m.type === 'attach') {
        const p = ev.ports[0];
        if (p) p.onmessage = e2 => this.push(e2.data);
      } else if (m && m.type === 'stats') {
        this.port.postMessage({ type: 'stats', avail: this.avail, underruns: this.underruns,
          dev: this.ratio / this.base - 1, priming: this.priming });
      }
    };
  }
  push(m) {
    if (!m || m.type !== 'pcm') return;
    const d = m.data, pan = m.pan | 0;
    // An underrun is a dry ring that audio refills within 300 ms (a gap
    // inside a transmission), not the silence after one ends.
    if (this.dryAt >= 0 && currentTime - this.dryAt < 0.3) this.underruns++;
    this.dryAt = -1;
    for (let i = 0; i < d.length; i++) {
      this.ring[this.w] = d[i];
      this.pan[this.w] = pan;
      this.w = (this.w + 1) % this.RING;
      if (this.avail < this.RING) this.avail++; else this.r = (this.r + 1) % this.RING;
    }
    if (this.priming && this.avail >= this.TARGET) this.priming = false;
  }
  process(inputs, outputs) {
    const out = outputs[0][0];
    if (!out) return true;
    const outR = outputs[0][1] || null;
    if (this.priming) { out.fill(0); if (outR) outR.fill(0); return true; }
    let dev = this.KP * (this.avail - this.TARGET);
    dev = Math.max(-this.MAXDEV, Math.min(this.MAXDEV, dev));
    this.ratio = this.base * (1 + dev);
    for (let i = 0; i < out.length; i++) {
      if (this.avail <= 1) {
        this.dryAt = currentTime; this.priming = true; out.fill(0, i); if (outR) outR.fill(0, i); return true;
      }
      const a = this.ring[this.r], b = this.ring[(this.r + 1) % this.RING];
      const v = a + (b - a) * this.frac, p = this.pan[this.r];
      if (outR) { out[i] = p > 0 ? 0 : v; outR[i] = p < 0 ? 0 : v; } else out[i] = v;
      this.frac += this.ratio;
      while (this.frac >= 1) {
        this.frac -= 1; this.r = (this.r + 1) % this.RING; this.avail--;
        if (this.avail <= 0) break;
      }
    }
    return true;
  }
}
registerProcessor('p25-audio', P25Audio);
`;

export const WORKER_SRC = `
let port = null, ws = null, url = null, backoff = 1000, timer = null, chunks = 0, lag = 0, last = 0;
let route = { map: {}, def: 0 }, tg = 0, pan = 0;
function panFor(t) { const p = route.map[t]; return p === undefined ? route.def : p; }
function report(force) {
  const now = Date.now();
  if (force || now - last > 250) { self.postMessage({ type: 'status', chunks, lag }); last = now; }
}
function connect() {
  clearTimeout(timer);
  ws = new WebSocket(url);
  ws.binaryType = 'arraybuffer';
  ws.onopen = () => { backoff = 1000; self.postMessage({ type: 'open' }); };
  ws.onmessage = ev => {
    if (typeof ev.data === 'string') {
      try {
        const c = JSON.parse(ev.data);
        if (c && c.type === 'lag') { lag += c.skipped || 0; report(true); }
        else if (c && c.type === 'meta') { tg = c.tg; pan = panFor(tg); }
      } catch (e) {}
      return;
    }
    const i16 = new Int16Array(ev.data);
    const f32 = new Float32Array(i16.length);
    for (let i = 0; i < i16.length; i++) f32[i] = i16[i] / 32768;
    if (port) port.postMessage({ type: 'pcm', data: f32, pan }, [f32.buffer]);
    chunks++;
    report(false);
  };
  ws.onclose = () => {
    self.postMessage({ type: 'close' });
    if (url) { timer = setTimeout(connect, backoff); backoff = Math.min(backoff * 2, 15000); }
  };
  ws.onerror = () => { try { ws.close(); } catch (e) {} };
}
self.onmessage = e => {
  const m = e.data;
  if (m.type === 'init') { port = m.port; url = m.url; if (m.route) route = m.route; connect(); }
  else if (m.type === 'route') { route = m.route; pan = panFor(tg); }
  else if (m.type === 'shutdown') { url = null; clearTimeout(timer); if (ws) try { ws.close(); } catch (e) {} }
};
`;
