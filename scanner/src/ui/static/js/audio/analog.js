// The analog receiver's audio, from /ws/analog: 16-bit mono at the rate its first text frame
// says, 20 ms a frame, none while the receiver's squelch is shut. One jitter buffer (ring.js,
// primed with 200 ms and resampled to the output's rate) run by a ScriptProcessorNode on the
// page's thread, which a page served over plain http has too. The volume is the header's. It
// plays while a page asks it to; a browser starts audio only after a click on the page
// (`resume`).

import { Ring, clip } from './ring.js';
import { wsUrl } from '../api.js';

const PATH = '/ws/analog';
// The buffer is primed with this much audio, seconds.
const PRIMED_S = 0.2;

class AnalogPlayer {
  constructor() {
    this.ctx = null; this.node = null; this.gain = null; this.ws = null; this.ring = null;
    this.playing = false; this.connected = false;
    this.volume = 1;
    this.rate = 0; this.frames = 0; this.lastFrameAt = 0; this.lagSkipped = 0;
    this.timer = null;
    this.listeners = new Set();
  }

  setVolume(v) {
    this.volume = Math.max(0, Math.min(1, Number(v) || 0));
    if (this.gain) this.gain.gain.value = this.volume;
  }

  // fn(status) now and at each change; returns what takes it off.
  onChange(fn) {
    this.listeners.add(fn);
    fn(this.status());
    return () => this.listeners.delete(fn);
  }

  status() {
    if (!this.playing) return { playing: false, label: '', detail: '' };
    const waiting = !!this.ctx && this.ctx.state === 'suspended';
    const gap = this.lastFrameAt ? Date.now() - this.lastFrameAt : -1;
    const hearing = gap >= 0 && gap < 500;
    const label = waiting ? 'Click to hear' : !this.connected ? 'Reconnecting' : hearing ? 'Playing' : 'Silent';
    const detail = waiting ? 'The browser plays audio after a click on the page' : [
      this.rate ? `${this.rate} Hz` : null,
      this.ring ? `buffer ${Math.round(this.ring.avail / this.rate * 1000)} ms` : null,
      `frames ${this.frames}`,
      this.ring && this.ring.underruns ? `underruns ${this.ring.underruns}` : null,
      this.lagSkipped ? `server lag ${this.lagSkipped}` : null,
    ].filter(Boolean).join(' · ');
    return { playing: true, label, detail, waiting, hearing };
  }

  emit() { const s = this.status(); for (const fn of this.listeners) fn(s); }

  // After a click: start the audio a page began without one.
  resume() {
    if (this.ctx && this.ctx.state === 'suspended') this.ctx.resume().catch(() => { /* needs another click */ });
  }

  start() {
    if (this.playing) return;
    const Ctor = window.AudioContext || window.webkitAudioContext;
    if (!Ctor) return;
    this.ctx = new Ctor();
    this.ctx.onstatechange = () => this.emit();
    this.resume();
    this.frames = 0; this.lagSkipped = 0; this.lastFrameAt = 0;
    this.gain = this.ctx.createGain();
    this.gain.gain.value = this.volume;
    this.gain.connect(this.ctx.destination);
    this.node = this.ctx.createScriptProcessor(1024, 0, 2);
    this.node.onaudioprocess = e => {
      const L = e.outputBuffer.getChannelData(0), R = e.outputBuffer.getChannelData(1);
      L.fill(0); R.fill(0);
      if (this.ring) this.ring.mix(L, R, performance.now() / 1000);
      clip(L); clip(R);
    };
    this.node.connect(this.gain);
    this.playing = true;
    this.open();
    this.timer = setInterval(() => this.emit(), 500);
    this.emit();
  }

  open() {
    const ws = this.ws = new WebSocket(wsUrl(PATH));
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => { this.connected = true; this.emit(); };
    ws.onmessage = ev => {
      if (!(ev.data instanceof ArrayBuffer)) {
        try {
          const m = JSON.parse(ev.data);
          if (m.type === 'lag') this.lagSkipped += m.skipped || 0;
          else if (m.type === 'format' && this.ctx) {
            // The buffer for this rate: the same loop in time as the lanes' 8 kHz one.
            this.rate = m.rate_hz;
            this.ring = new Ring(this.rate / this.ctx.sampleRate);
            this.ring.TARGET = Math.round(this.rate * PRIMED_S);
            this.ring.KP *= 8000 / this.rate;
          }
        } catch { /* not ours */ }
        return;
      }
      if (!this.ring) return;
      this.ring.write(new Int16Array(ev.data), 0, performance.now() / 1000, 1 / 32768);
      this.frames++;
      this.lastFrameAt = Date.now();
    };
    ws.onclose = () => {
      this.connected = false;
      this.emit();
      if (this.playing) setTimeout(() => { if (this.playing && this.ws === ws) this.open(); }, 2000);
    };
  }

  stop() {
    this.playing = false;
    this.connected = false;
    clearInterval(this.timer);
    if (this.ws) { try { this.ws.close(); } catch { /* gone */ } }
    if (this.node) {
      try { this.node.disconnect(); } catch { /* gone */ }
      this.node.onaudioprocess = null;
    }
    if (this.ctx) { try { this.ctx.close(); } catch { /* gone */ } }
    this.ws = null; this.node = null; this.ctx = null; this.gain = null; this.ring = null;
    this.emit();
  }
}

export const analogPlayer = new AnalogPlayer();
