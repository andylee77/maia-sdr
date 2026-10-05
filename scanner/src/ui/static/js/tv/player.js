// The TV's sound in the page. A Worker fetches a channel's stream and plays it through the ATSC
// player's WebAssembly module (scanner/player); the sound goes to an AudioWorklet ring in a secure
// context, or, on plain http to the unit (where browsers hide AudioWorklet), to the same ring on
// the main thread under a ScriptProcessorNode, as the radio's audio does (../audio/player.js).
// The module is compiled once and handed to each Worker.

import { StereoRing } from './ring.js';
import { WORKLET_SRC, WORKER_SRC } from './sources.js';

const MODULE_URL = new URL('../../player/atsc_player.wasm', import.meta.url);
// ATSC's sound is 48 kHz (A/53 part 5).
const RATE = 48000;

let compiled = null;
function playerModule() {
  if (!compiled) {
    compiled = WebAssembly.compileStreaming
      ? WebAssembly.compileStreaming(fetch(MODULE_URL))
      : fetch(MODULE_URL).then(r => r.arrayBuffer()).then(b => WebAssembly.compile(b));
    compiled.catch(() => { compiled = null; });
  }
  return compiled;
}

// The address of a programme's stream, for this page's own player and for VLC.
export function streamUrl(rf, program) {
  return new URL(`/api/v1/atsc/stream/${rf}?program=${program}`, location.href).href;
}

class TvPlayer {
  constructor() {
    this.ctx = null; this.node = null; this.worker = null; this.ring = null; this.timer = null;
    this.mode = null; this.playing = null; this.error = ''; this.open = false;
    this.frames = 0; this.info = null; this.stats = null;
    this.listeners = new Set();
  }

  // fn(status) now and at each change; returns its removal.
  onChange(fn) {
    this.listeners.add(fn);
    fn(this.status());
    return () => this.listeners.delete(fn);
  }

  emit() { const s = this.status(); for (const fn of this.listeners) fn(s); }

  // { playing: {rf, program} or null, label, detail }.
  status() {
    if (!this.playing) return { playing: null, label: this.error ? `Stopped: ${this.error}` : '', detail: '' };
    const st = this.stats || {};
    const waiting = this.ctx && this.ctx.state === 'suspended';
    const label = this.error ? `Stopped: ${this.error}` : waiting ? 'Click to hear' : !this.open ? 'Connecting'
      : !st.started || st.priming ? 'Buffering' : 'Playing';
    const a = this.info && this.info.audio;
    const detail = [
      a ? `AC-3 ${a.channels} · ${a.kbps} kbit/s` : null,
      this.mode === 'worklet' ? 'AudioWorklet' : this.mode === 'spn' ? 'ScriptProcessor (http)' : null,
      st.avail != null ? `buffer ${Math.round(st.avail / RATE * 1000)} ms` : null,
      st.underruns ? `underruns ${st.underruns}` : null,
      this.info && this.info.lost_frames ? `frames lost ${this.info.lost_frames}` : null,
    ].filter(Boolean).join(' · ');
    return { playing: this.playing, label, detail };
  }

  // After a click: start the sound a start without one left waiting.
  resume() {
    if (this.ctx && this.ctx.state === 'suspended') this.ctx.resume().catch(() => { /* needs another click */ });
  }

  async start(rf, program) {
    this.stop();
    const Ctor = window.AudioContext || window.webkitAudioContext;
    if (!Ctor) { this.error = 'this browser has no Web Audio'; this.emit(); return; }
    this.playing = { rf, program };
    this.error = ''; this.open = false; this.frames = 0; this.info = null; this.stats = null;
    try {
      this.ctx = new Ctor({ sampleRate: RATE });
    } catch {
      this.ctx = new Ctor();
    }
    this.ctx.onstatechange = () => this.emit();
    this.resume();
    const ctx = this.ctx;
    try {
      const module = await playerModule();
      if (ctx !== this.ctx) return;
      const wurl = URL.createObjectURL(new Blob([WORKER_SRC], { type: 'text/javascript' }));
      const worker = this.worker = new Worker(wurl);
      URL.revokeObjectURL(wurl);
      worker.onmessage = ev => this.fromWorker(ev.data || {});
      const init = { type: 'init', module, url: streamUrl(rf, program), program };
      if (ctx.audioWorklet) {
        this.mode = 'worklet';
        const src = URL.createObjectURL(new Blob([WORKLET_SRC], { type: 'text/javascript' }));
        try { await ctx.audioWorklet.addModule(src); } finally { URL.revokeObjectURL(src); }
        if (ctx !== this.ctx) return;
        this.node = new AudioWorkletNode(ctx, 'tv-audio', { outputChannelCount: [2], processorOptions: { inRate: RATE } });
        this.node.port.onmessage = ev => { if (ev.data && ev.data.type === 'stats') { this.stats = ev.data; this.emit(); } };
        const mc = new MessageChannel();
        this.node.port.postMessage({ type: 'attach' }, [mc.port2]);
        worker.postMessage({ ...init, port: mc.port1 }, [mc.port1]);
      } else {
        this.mode = 'spn';
        const ring = this.ring = new StereoRing(RATE, ctx.sampleRate);
        this.node = ctx.createScriptProcessor(2048, 0, 2);
        this.node.onaudioprocess = e => {
          const L = e.outputBuffer.getChannelData(0), R = e.outputBuffer.getChannelData(1);
          L.fill(0); R.fill(0);
          ring.mix(L, R);
        };
        worker.postMessage(init);
      }
      this.node.connect(ctx.destination);
      this.timer = setInterval(() => {
        if (this.mode === 'worklet' && this.node) this.node.port.postMessage({ type: 'stats' });
        if (this.ring) {
          const r = this.ring;
          this.stats = { avail: r.avail, underruns: r.underruns, priming: r.priming, started: r.started, pts: r.pts() };
          this.emit();
        }
      }, 500);
    } catch (e) {
      this.fail(`the player did not start: ${(e && e.message) || e}`);
      return;
    }
    this.emit();
  }

  fromWorker(m) {
    if (m.type === 'pcm') { if (this.ring) this.ring.write(m.data, m.pts); }
    else if (m.type === 'open') { this.open = true; this.emit(); }
    else if (m.type === 'status') { this.frames = m.frames; this.info = m.info; this.emit(); }
    else if (m.type === 'error') this.fail(m.error);
  }

  // Stop, keeping why for the page to show.
  fail(why) {
    this.stop();
    this.error = why;
    this.emit();
  }

  stop() {
    clearInterval(this.timer);
    if (this.worker) { try { this.worker.postMessage({ type: 'shutdown' }); this.worker.terminate(); } catch { /* gone */ } }
    if (this.node) {
      try { this.node.disconnect(); } catch { /* gone */ }
      if ('onaudioprocess' in this.node) this.node.onaudioprocess = null;
    }
    if (this.ctx) { try { this.ctx.close(); } catch { /* gone */ } }
    this.ctx = null; this.node = null; this.worker = null; this.ring = null; this.timer = null; this.mode = null;
    this.playing = null; this.error = ''; this.open = false; this.stats = null;
    this.emit();
  }
}

export const tv = new TvPlayer();
