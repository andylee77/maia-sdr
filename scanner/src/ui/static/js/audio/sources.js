// Source text of the AudioWorklet processor and the /ws/audio Worker, loaded through Blob URLs
// (no extra routes):
//
//   net -> Worker (own thread, WebSocket) -> MessagePort -> AudioWorklet
//
// The jitter buffer, resampling PLL and per-sample pan are `Ring` (ring.js), whose source text is
// injected into the worklet. The server names each lane's talkgroup in a {"type":"meta"} text
// frame before its audio; the Worker maps it to a pan (player.js `routeFromProfile`) and tags each
// block. Binary frames start with a 4-byte header [lane, 0, 0, 0]; the worklet keeps one ring per
// lane and mixes them, so a call on the left and another on the right play at the same time.

import { Ring, clip } from './ring.js';

export const WORKLET_SRC = Ring.toString() + '\n' + clip.toString() + `
class P25Audio extends AudioWorkletProcessor {
  constructor() {
    super();
    this.rings = [new Ring(8000 / sampleRate), new Ring(8000 / sampleRate)];
    this.port.onmessage = ev => {
      const m = ev.data;
      if (m && m.type === 'attach') {
        const p = ev.ports[0];
        if (p) p.onmessage = e2 => this.push(e2.data);
      } else if (m && m.type === 'norm') {
        for (const r of this.rings) r.norm = !!m.on;
      } else if (m && m.type === 'stats') {
        const [a, b] = this.rings;
        this.port.postMessage({ type: 'stats', avail: Math.max(a.avail, b.avail),
          underruns: a.underruns + b.underruns, dev: a.ratio / a.base - 1,
          priming: a.priming && b.priming });
      }
    };
  }
  push(m) {
    if (!m || m.type !== 'pcm') return;
    this.rings[m.lane === 1 ? 1 : 0].write(m.data, m.pan | 0, currentTime);
  }
  process(inputs, outputs) {
    const out = outputs[0][0];
    if (!out) return true;
    const outR = outputs[0][1] || null;
    out.fill(0);
    if (outR) outR.fill(0);
    for (const ring of this.rings) ring.mix(out, outR, currentTime);
    clip(out);
    if (outR) clip(outR);
    return true;
  }
}
registerProcessor('p25-audio', P25Audio);
`;

export const WORKER_SRC = `
let port = null, ws = null, url = null, backoff = 1000, timer = null, chunks = 0, lag = 0, last = 0;
let route = { map: {}, def: 0 }, tgs = [0, 0], pans = [0, 0];
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
        else if (c && c.type === 'meta') { const l = c.lane === 1 ? 1 : 0; tgs[l] = c.tg; pans[l] = panFor(c.tg); }
      } catch (e) {}
      return;
    }
    const lane = new Uint8Array(ev.data, 0, 1)[0] === 1 ? 1 : 0;
    const i16 = new Int16Array(ev.data, 4);
    const f32 = new Float32Array(i16.length);
    for (let i = 0; i < i16.length; i++) f32[i] = i16[i] / 32768;
    if (port) port.postMessage({ type: 'pcm', lane, data: f32, pan: pans[lane] }, [f32.buffer]);
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
  else if (m.type === 'route') { route = m.route; pans = tgs.map(panFor); }
  else if (m.type === 'shutdown') { url = null; clearTimeout(timer); if (ws) try { ws.close(); } catch (e) {} }
};
`;
