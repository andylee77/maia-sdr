// Live traffic audio from /ws/audio (8 kHz 16-bit mono, 20 ms frames).
//
// Secure context (https / localhost): AudioWorklet fed by a Worker
// that owns the WebSocket, so page work can never starve playback.
// Plain http to the radio's IP (the usual case): browsers hide
// AudioWorklet there, so a ScriptProcessorNode runs the same ring +
// PLL on the main thread.

import { WORKLET_SRC, WORKER_SRC } from './sources.js';
import { wsUrl } from '../api.js';

const RING = 16384;
const TARGET = 1200; // 150 ms @ 8 kHz
const KP = 4e-5;
const MAXDEV = 0.005;

class Player {
  constructor() {
    this.ctx = null; this.node = null; this.gain = null;
    this.worker = null; this.ws = null;
    this.mode = null; this.playing = false;
    this.chunks = 0; this.lagSkipped = 0; this.underruns = 0; this.bufMs = 0;
    this.lastChunkAt = 0; this.timer = null; this.connected = false;
    this.listeners = new Set();
    this.spn = null;
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
      if (this.mode === 'spn' && this.spn) { this.bufMs = (this.spn.avail / 8) | 0; this.underruns = this.spn.underruns; }
      this.emit();
    }, 500);
    this.emit();
  }

  async startWorklet() {
    this.mode = 'worklet';
    const url = URL.createObjectURL(new Blob([WORKLET_SRC], { type: 'text/javascript' }));
    try { await this.ctx.audioWorklet.addModule(url); } finally { URL.revokeObjectURL(url); }
    this.node = new AudioWorkletNode(this.ctx, 'p25-audio');
    this.node.port.onmessage = ev => {
      const m = ev.data;
      if (m && m.type === 'stats') { this.bufMs = (m.avail / 8) | 0; this.underruns = m.underruns; }
    };
    this.node.connect(this.gain);
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
    this.worker.postMessage({ type: 'init', url: wsUrl('/ws/audio'), port: mc.port1 }, [mc.port1]);
  }

  startSpn() {
    this.mode = 'spn';
    const s = this.spn = {
      ring: new Float32Array(RING), w: 0, r: 0, avail: 0, frac: 0,
      base: 8000 / this.ctx.sampleRate, priming: true, underruns: 0, dryAt: 0,
    };
    this.node = this.ctx.createScriptProcessor(1024, 0, 1);
    this.node.onaudioprocess = e => spnProcess(s, e.outputBuffer.getChannelData(0));
    this.node.connect(this.gain);
    this.openSpnSocket();
  }

  openSpnSocket() {
    const ws = this.ws = new WebSocket(wsUrl('/ws/audio'));
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => { this.connected = true; this.emit(); };
    ws.onmessage = ev => {
      if (!(ev.data instanceof ArrayBuffer)) {
        try { const c = JSON.parse(ev.data); if (c.type === 'lag') this.lagSkipped += c.skipped || 0; } catch { /* ignore */ }
        return;
      }
      const i16 = new Int16Array(ev.data);
      spnWrite(this.spn, i16);
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

function spnWrite(s, i16) {
  if (!s) return;
  // Underrun = dry ring refilled within 300 ms (see sources.js).
  if (s.dryAt && performance.now() - s.dryAt < 300) s.underruns++;
  s.dryAt = 0;
  for (let i = 0; i < i16.length; i++) {
    s.ring[s.w] = i16[i] / 32768;
    s.w = (s.w + 1) % RING;
    if (s.avail < RING) s.avail++; else s.r = (s.r + 1) % RING;
  }
  if (s.priming && s.avail >= TARGET) s.priming = false;
}

function spnProcess(s, out) {
  if (s.priming) { out.fill(0); return; }
  let dev = KP * (s.avail - TARGET);
  dev = Math.max(-MAXDEV, Math.min(MAXDEV, dev));
  const ratio = s.base * (1 + dev);
  for (let i = 0; i < out.length; i++) {
    // Dry: re-prime at once (see sources.js) so the next transmission
    // starts with the full 150 ms buffer.
    if (s.avail <= 1) { s.dryAt = performance.now(); s.priming = true; out.fill(0, i); return; }
    const a = s.ring[s.r], b = s.ring[(s.r + 1) % RING];
    out[i] = a + (b - a) * s.frac;
    s.frac += ratio;
    while (s.frac >= 1) {
      s.frac -= 1; s.r = (s.r + 1) % RING; s.avail--;
      if (s.avail <= 0) break;
    }
  }
}

export const player = new Player();
