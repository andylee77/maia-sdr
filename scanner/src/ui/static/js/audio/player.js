// Live audio from /ws/audio (8 kHz 16-bit mono, 20 ms frames, one stream per lane, mixed).
//
// Secure context (https / localhost): AudioWorklet fed by a Worker
// that owns the WebSocket, so page work can never starve playback.
// Plain http to the radio's IP (the usual case): browsers hide
// AudioWorklet there, so a ScriptProcessorNode runs the same rings
// (ring.js) and gate (gate.js) on the main thread.
//
// Each lane plays at its own volume under the master volume. In alerts-only mode the gate holds a
// lane's call back until an alert tone opens its talkgroup; the player keeps when each opened
// talkgroup closes, for the page to show. A browser starts audio only after a click on the page:
// a player started without one waits (`resume`).

import { WORKLET_SRC, WORKER_SRC } from './sources.js';
import { Ring, clip } from './ring.js';
import { AlertGate } from './gate.js';
import { wsUrl } from '../api.js';

const AUDIO_PATH = '/ws/audio?v=2';

// The speaker of a lane's call, from its meta frame, as a pan: -1 left, 0 both, +1 right.
const PAN = { left: -1, both: 0, right: 1 };

class Player {
  constructor() {
    this.ctx = null; this.node = null; this.gain = null;
    this.worker = null; this.ws = null;
    this.mode = null; this.playing = false;
    this.chunks = 0; this.lagSkipped = 0; this.underruns = 0; this.bufMs = 0;
    this.lastChunkAt = 0; this.timer = null; this.connected = false;
    this.listeners = new Set();
    this.alertListeners = new Set();
    this.spn = null;
    this.volume = 1; // linear gain, 0..2
    this.laneVol = [1, 1];
    this.pans = [0, 0];
    this.gate = new AlertGate();
    this.alertsOnly = false;
    this.windowMs = 120000;
    // Talkgroup -> when the alert window it opened ends (Date.now() ms).
    this.opened = new Map();
  }

  setVolume(v) {
    this.volume = Math.max(0, Math.min(2, Number(v) || 0));
    if (this.gain) this.gain.gain.value = this.volume;
  }

  // One lane's volume (0: Traffic 1), 0..2; 0 mutes it.
  setLaneVolume(lane, v) {
    this.laneVol[lane] = Math.max(0, Math.min(2, Number(v) || 0));
    if (this.mode === 'worklet' && this.node) this.node.port.postMessage({ type: 'vol', lane, v: this.laneVol[lane] });
    if (this.spn) this.spn[lane].vol = this.laneVol[lane];
  }

  // Volume normalization (ring.js) on every lane.
  setNormalize(on) {
    this.normalize = !!on;
    if (this.mode === 'worklet' && this.node) this.node.port.postMessage({ type: 'norm', on: this.normalize });
    if (this.spn) for (const r of this.spn) r.norm = this.normalize;
  }

  // Alerts-only listening, each alert opening its talkgroup for `windowMs`.
  setAlertMode(on, windowMs) {
    this.alertsOnly = !!on;
    this.windowMs = windowMs;
    this.gate.setMode(this.alertsOnly, windowMs);
    if (this.worker) this.worker.postMessage({ type: 'gate', on: this.alertsOnly, windowMs });
    this.emit();
  }

  onChange(fn) { this.listeners.add(fn); fn(this.status()); }

  // fn(alert) for each alert tone heard while listening ({lane, tg, call_id, kind, tones_hz}).
  onAlert(fn) { this.alertListeners.add(fn); return () => this.alertListeners.delete(fn); }

  // The talkgroups an alert opened, with the ms each has left.
  openTalkgroups() {
    const now = Date.now();
    return [...this.opened].filter(([, until]) => until > now).map(([tg, until]) => [tg, until - now]);
  }

  status() {
    if (!this.playing) return { playing: false, label: '', detail: '' };
    const gap = this.lastChunkAt ? Date.now() - this.lastChunkAt : -1;
    const waiting = this.ctx && this.ctx.state === 'suspended';
    const label = waiting ? 'Click to hear'
      : !this.connected ? 'Reconnecting'
        : this.alertsOnly ? 'Alerts only'
          : gap < 0 || gap > 1500 ? 'Listening' : 'Playing';
    const detail = waiting ? 'The browser plays audio after a click on the page' : [
      this.mode === 'worklet' ? 'AudioWorklet' : 'ScriptProcessor (http)',
      'buffer ' + this.bufMs + ' ms',
      'chunks ' + this.chunks,
      this.underruns ? 'underruns ' + this.underruns : null,
      this.lagSkipped ? 'server lag ' + this.lagSkipped : null,
    ].filter(Boolean).join(' · ');
    return { playing: true, label, detail, waiting, open: this.openTalkgroups() };
  }

  emit() { const s = this.status(); for (const fn of this.listeners) fn(s); }

  heard(alert) {
    this.opened.set(alert.tg, Date.now() + this.windowMs);
    for (const fn of this.alertListeners) fn(alert);
    this.emit();
  }

  toggle() { if (this.playing) this.stop(); else this.start(); }

  // After a click: start the audio a page load began without one.
  resume() {
    if (this.ctx && this.ctx.state === 'suspended') this.ctx.resume().catch(() => { /* needs another click */ });
  }

  async start() {
    if (this.playing) return;
    const Ctor = window.AudioContext || window.webkitAudioContext;
    if (!Ctor) return;
    this.ctx = new Ctor();
    this.ctx.onstatechange = () => this.emit();
    // Not awaited: without a click on the page it waits until one.
    this.resume();
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
    this.laneVol.forEach((v, lane) => this.setLaneVolume(lane, v));
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
    this.node = new AudioWorkletNode(this.ctx, 'lane-audio', { outputChannelCount: [2] });
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
      else if (m.type === 'alert') this.heard(m.alert);
    };
    this.worker.postMessage({ type: 'gate', on: this.alertsOnly, windowMs: this.windowMs });
    this.worker.postMessage({ type: 'init', url: wsUrl(AUDIO_PATH), port: mc.port1 }, [mc.port1]);
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
    const write = (lane, b) => { if (this.spn) this.spn[lane].write(b.data, b.pan, performance.now() / 1000, 1 / 32768); };
    ws.onopen = () => { this.connected = true; this.emit(); };
    ws.onmessage = ev => {
      if (!(ev.data instanceof ArrayBuffer)) {
        try {
          const c = JSON.parse(ev.data);
          const lane = c.lane === 1 ? 1 : 0;
          if (c.type === 'lag') this.lagSkipped += c.skipped || 0;
          else if (c.type === 'meta') {
            this.pans[lane] = PAN[c.speaker] ?? PAN.both;
            this.gate.call(lane, c.tg, c.call_id, Date.now());
          } else if (c.type === 'alert') {
            for (const b of this.gate.alert(lane, c.tg, c.call_id, Date.now())) write(lane, b);
            this.heard(c);
          }
        } catch { /* ignore */ }
        return;
      }
      const lane = new Uint8Array(ev.data, 0, 1)[0] === 1 ? 1 : 0;
      const block = { data: new Int16Array(ev.data, 4), pan: this.pans[lane] };
      if (this.gate.pass(lane, block)) write(lane, block);
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
    this.gate = new AlertGate();
    this.gate.setMode(this.alertsOnly, this.windowMs);
    this.emit();
  }
}

export const player = new Player();
