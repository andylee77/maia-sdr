// Source text of the TV's AudioWorklet processor and Worker, loaded through Blob URLs:
//
//   net -> Worker (fetch, the player's WebAssembly module) -> MessagePort -> AudioWorklet
//                                                           -> the page (pictures)
//
// The Worker fetches a channel's stream (/api/v1/atsc/stream/{rf}?program=), writes it into the
// player (scanner/player: the transport stream, AC-3, MPEG-2 video) and sends each syncframe's
// sound, two channels interleaved with its presentation time, to the worklet's `StereoRing`
// (ring.js, whose source text is injected), or to the page when there is no worklet; each
// picture's planes, size, time and flags go to the page. It reports the player's description
// (programmes, the formats, frames lost) twice a second. The worklet reports, when asked, its
// ring's state and the time of the sample it plays next with its own clock, for the pictures to
// follow the sound.

import { StereoRing } from './ring.js';

export const WORKLET_SRC = StereoRing.toString() + `
class TvAudio extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.ring = new StereoRing(options.processorOptions.inRate, sampleRate);
    this.port.onmessage = ev => {
      const m = ev.data;
      if (m && m.type === 'attach') {
        const p = ev.ports[0];
        if (p) p.onmessage = e2 => { if (e2.data && e2.data.type === 'pcm') this.ring.write(e2.data.data, e2.data.pts); };
      } else if (m && m.type === 'stats') {
        const r = this.ring;
        this.port.postMessage({ type: 'stats', avail: r.avail, underruns: r.underruns, priming: r.priming, started: r.started, pts: r.pts(), at: currentTime });
      }
    };
  }
  process(inputs, outputs) {
    const [L, R] = outputs[0];
    if (!L || !R) return true;
    L.fill(0);
    R.fill(0);
    this.ring.mix(L, R);
    return true;
  }
}
registerProcessor('tv-audio', TvAudio);
`;

export const WORKER_SRC = `
let ex = null, h = 0, port = null, url = null, abort = null, frames = 0, last = 0;
function text(ptr, n) { return new TextDecoder().decode(new Uint8Array(ex.memory.buffer, ptr, n)); }
function describe() {
  const n = ex.player_describe(h);
  return JSON.parse(text(ex.player_text(h), n));
}
function feed(bytes) {
  const at = ex.player_input(h, bytes.length);
  new Uint8Array(ex.memory.buffer, at, bytes.length).set(bytes);
  ex.player_push(h);
  for (let n; (n = ex.player_sound(h)) > 0;) {
    const data = new Float32Array(ex.memory.buffer, ex.player_sound_ptr(h), 2 * n).slice();
    const block = { type: 'pcm', data, pts: ex.player_sound_pts(h) };
    if (port) port.postMessage(block, [data.buffer]); else self.postMessage(block, [data.buffer]);
    frames++;
  }
  while (ex.player_picture(h)) {
    const stride = ex.player_picture_stride(h), rows = ex.player_picture_rows(h);
    const plane = (k, n) => new Uint8Array(ex.memory.buffer, ex.player_picture_plane(h, k), n).slice().buffer;
    const pic = {
      type: 'picture', y: plane(0, stride * rows), cb: plane(1, stride * rows / 4), cr: plane(2, stride * rows / 4),
      stride, rows, width: ex.player_picture_width(h), height: ex.player_picture_height(h),
      pts: ex.player_picture_pts(h), flags: ex.player_picture_flags(h),
    };
    self.postMessage(pic, [pic.y, pic.cb, pic.cr]);
  }
  const now = Date.now();
  if (now - last > 500) { last = now; self.postMessage({ type: 'status', frames, info: describe() }); }
}
async function run() {
  abort = new AbortController();
  try {
    const res = await fetch(url, { signal: abort.signal, cache: 'no-store' });
    if (!res.ok) {
      let error = 'HTTP ' + res.status;
      try { error = (await res.json()).error || error; } catch (e) {}
      self.postMessage({ type: 'error', error });
      return;
    }
    self.postMessage({ type: 'open' });
    const reader = res.body.getReader();
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      feed(value);
    }
    self.postMessage({ type: 'error', error: 'the stream ended' });
  } catch (e) {
    if (url) self.postMessage({ type: 'error', error: String((e && e.message) || e) });
  }
}
self.onmessage = async e => {
  const m = e.data;
  if (m.type === 'init') {
    port = m.port || null;
    url = m.url;
    try {
      ex = (await WebAssembly.instantiate(m.module, {})).exports;
      h = ex.player_new(m.program || 0);
    } catch (err) {
      self.postMessage({ type: 'error', error: 'the player did not load: ' + ((err && err.message) || err) });
      return;
    }
    run();
  } else if (m.type === 'shutdown') {
    url = null;
    if (abort) abort.abort();
  }
};
`;
