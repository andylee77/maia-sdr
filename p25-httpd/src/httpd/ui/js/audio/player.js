// Live traffic audio from /ws/audio?v=2 (8 kHz 16-bit mono, 20 ms
// frames, change 066: one stream per traffic chain, mixed).
//
// Secure context (https / localhost): AudioWorklet fed by a Worker
// that owns the WebSocket, so page work can never starve playback.
// Plain http to the radio's IP (the usual case): browsers hide
// AudioWorklet there, so a ScriptProcessorNode runs the same rings
// (ring.js) on the main thread.

import { WORKLET_SRC, WORKER_SRC } from './sources.js';
import { Ring, clip } from './ring.js';
import { wsUrl } from '../api.js';

// Change 066: every traffic chain, lane-tagged frames.
const AUDIO_PATH = '/ws/audio?v=2';

const PAN = { left: -1, both: 0, right: 1 };

// Change 063: speaker routing from the radio's talkgroup groups
// (/api/ui/settings: tg_groups in priority order, speakers.left /
// .right group names, speakers.other for ungrouped talkgroups) as
// {map: {tg: pan}, def: pan}, pan -1 left / 0 both / +1 right. A
// talkgroup in several groups takes the first, as the follower does;
// groups on neither side are not followed, so their pan is moot.
export function routeFromSettings(doc) {
  const s = doc && doc.settings;
  const map = {};
  if (!s) return { map, def: PAN.both };
  const sp = s.speakers || {};
  const side = name => ((sp.left || []).includes(name) ? PAN.left
    : (sp.right || []).includes(name) ? PAN.right : PAN.both);
  for (const g of s.tg_groups || []) {
    const p = side(g.name);
    for (const tg of g.tgs || []) if (!(tg in map)) map[tg] = p;
  }
  return { map, def: PAN[sp.other] ?? PAN.both };
}

class Player {
  constructor() {
    this.ctx = null; this.node = null; this.gain = null;
    this.worker = null; this.ws = null;
    this.mode = null; this.playing = false;
    this.chunks = 0; this.lagSkipped = 0; this.underruns = 0; this.bufMs = 0;
    this.lastChunkAt = 0; this.timer = null; this.connected = false;
    this.listeners = new Set();
    this.spn = null;
    // Change 062: volume (linear gain, 0..2) and speaker routing.
    this.volume = 1;
    this.route = { map: {}, def: 0 };
    this.tgs = [0, 0];
  }

  setVolume(v) {
    this.volume = Math.max(0, Math.min(2, Number(v) || 0));
    if (this.gain) this.gain.gain.value = this.volume;
  }

  // Change 067: volume normalization (ring.js) on every chain.
  setNormalize(on) {
    this.normalize = !!on;
    if (this.mode === 'worklet' && this.node) this.node.port.postMessage({ type: 'norm', on: this.normalize });
    if (this.spn) for (const r of this.spn) r.norm = this.normalize;
  }

  setRoute(route) {
    this.route = route;
    if (this.worker) this.worker.postMessage({ type: 'route', route });
  }

  panFor(tg) {
    const p = this.route.map[tg];
    return p === undefined ? this.route.def : p;
  }

  onChange(fn) { this.listeners.add(fn); fn(this.status()); }

  status() {
    if (!this.playing) return { playing: false, label: '', detail: '' };
    const gap = this.lastChunkAt ? Date.now() - this.lastChunkAt : -1;
    const label = !this.connected ? 'Reconnecting' : gap < 0 || gap > 1500 ? 'Listening' : 'Playing';
    const detail = [
      this.mode === 'worklet' ? 'AudioWorklet' : 'ScriptProcessor (http)',
      'buffer ' + this.bufMs + ' ms',
      'chunks ' + this.chunks,
      this.underruns ? 'underruns ' + this.underruns : null,
      this.lagSkipped ? 'server lag ' + this.lagSkipped : null,
    ].filter(Boolean).join(' · ');
    return { playing: true, label, detail };
  }

  emit() { const s = this.status(); for (const fn of this.listeners) fn(s); }

  toggle() { if (this.playing) this.stop(); else this.start(); }

  async start() {
    const Ctor = window.AudioContext || window.webkitAudioContext;
    if (!Ctor) return;
    this.ctx = new Ctor();
    if (this.ctx.state === 'suspended') { try { await this.ctx.resume(); } catch { /* user gesture pending */ } }
    this.chunks = 0; this.underruns = 0; this.lagSkipped = 0; this.lastChunkAt = 0;
    this.gain = this.ctx.createGain();
    this.gain.gain.value = this.volume;
    this.gain.connect(this.ctx.destination);
    this.playing = true;
    try {
      if (this.ctx.audioWorklet) await this.startWorklet();
      else this.startSpn();
    } catch (e) {
      console.error('audio start failed', e);
      this.stop();
      return;
    }
    this.timer = setInterval(() => {
      if (this.mode === 'worklet' && this.node) this.node.port.postMessage({ type: 'stats' });
      if (this.mode === 'spn' && this.spn) {
        this.bufMs = (Math.max(...this.spn.map(r => r.avail)) / 8) | 0;
        this.underruns = this.spn.reduce((n, r) => n + r.underruns, 0);
      }
      this.emit();
    }, 500);
    this.emit();
  }

  async startWorklet() {
    this.mode = 'worklet';
    const url = URL.createObjectURL(new Blob([WORKLET_SRC], { type: 'text/javascript' }));
    try { await this.ctx.audioWorklet.addModule(url); } finally { URL.revokeObjectURL(url); }
    this.node = new AudioWorkletNode(this.ctx, 'p25-audio', { outputChannelCount: [2] });
    this.node.port.onmessage = ev => {
      const m = ev.data;
      if (m && m.type === 'stats') { this.bufMs = (m.avail / 8) | 0; this.underruns = m.underruns; }
    };
    this.node.connect(this.gain);
    this.node.port.postMessage({ type: 'norm', on: !!this.normalize });
    const wurl = URL.createObjectURL(new Blob([WORKER_SRC], { type: 'text/javascript' }));
    this.worker = new Worker(wurl);
    URL.revokeObjectURL(wurl);
    const mc = new MessageChannel();
    this.node.port.postMessage({ type: 'attach' }, [mc.port2]);
    this.worker.onmessage = ev => {
      const m = ev.data || {};
      if (m.type === 'open') { this.connected = true; this.emit(); }
      else if (m.type === 'close') { this.connected = false; this.emit(); }
      else if (m.type === 'status') { this.chunks = m.chunks; this.lagSkipped = m.lag; this.lastChunkAt = Date.now(); }
    };
    this.worker.postMessage({ type: 'init', url: wsUrl(AUDIO_PATH), port: mc.port1, route: this.route }, [mc.port1]);
  }

  startSpn() {
    this.mode = 'spn';
    const base = 8000 / this.ctx.sampleRate;
    const rings = this.spn = [new Ring(base), new Ring(base)];
    for (const r of rings) r.norm = !!this.normalize;
    this.node = this.ctx.createScriptProcessor(1024, 0, 2);
    this.node.onaudioprocess = e => {
      const L = e.outputBuffer.getChannelData(0), R = e.outputBuffer.getChannelData(1);
      L.fill(0); R.fill(0);
      const now = performance.now() / 1000;
      for (const ring of rings) ring.mix(L, R, now);
      clip(L); clip(R);
    };
    this.node.connect(this.gain);
    this.openSpnSocket();
  }

  openSpnSocket() {
    const ws = this.ws = new WebSocket(wsUrl(AUDIO_PATH));
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => { this.connected = true; this.emit(); };
    ws.onmessage = ev => {
      if (!(ev.data instanceof ArrayBuffer)) {
        try {
          const c = JSON.parse(ev.data);
          if (c.type === 'lag') this.lagSkipped += c.skipped || 0;
          else if (c.type === 'meta') this.tgs[c.lane === 1 ? 1 : 0] = c.tg;
        } catch { /* ignore */ }
        return;
      }
      const lane = new Uint8Array(ev.data, 0, 1)[0] === 1 ? 1 : 0;
      const i16 = new Int16Array(ev.data, 4);
      if (this.spn) this.spn[lane].write(i16, this.panFor(this.tgs[lane]), performance.now() / 1000, 1 / 32768);
      this.chunks++;
      this.lastChunkAt = Date.now();
    };
    ws.onclose = () => {
      this.connected = false;
      this.emit();
      if (this.playing && this.mode === 'spn') setTimeout(() => { if (this.playing) this.openSpnSocket(); }, 2000);
    };
  }

  stop() {
    this.playing = false;
    this.connected = false;
    clearInterval(this.timer);
    if (this.worker) { try { this.worker.postMessage({ type: 'shutdown' }); this.worker.terminate(); } catch { /* gone */ } }
    if (this.ws) { try { this.ws.close(); } catch { /* gone */ } }
    if (this.node) {
      try { this.node.disconnect(); } catch { /* gone */ }
      if ('onaudioprocess' in this.node) this.node.onaudioprocess = null;
    }
    if (this.ctx) { try { this.ctx.close(); } catch { /* gone */ } }
    this.worker = null; this.ws = null; this.node = null; this.ctx = null; this.gain = null; this.spn = null; this.mode = null;
    this.emit();
  }
}

export const player = new Player();
