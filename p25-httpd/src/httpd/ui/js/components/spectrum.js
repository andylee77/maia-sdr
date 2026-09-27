// Spectrum card: wideband (HDL FFT over the whole AD9361 band, with
// the control and traffic frequencies marked) or narrowband (PS FFT of
// one DDC output). Polls only while mounted, 1 Hz.

import { h, setText, card } from '../dom.js';
import { api } from '../api.js';
import { store } from '../store.js';

const SOURCES = [
  { id: 'wide', label: 'Wideband' },
  { id: 'control', label: 'Control DDC' },
  { id: 'traffic', label: 'Traffic DDC' },
];
const POLL_MS = 1000;

function css(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || '#888';
}

export function spectrumCard() {
  const c = card('Spectrum');
  let source = 'wide';
  const chips = SOURCES.map(s => {
    const b = h('button', { class: 'chip' + (s.id === source ? ' on' : ''), type: 'button', text: s.label });
    b.addEventListener('click', () => {
      source = s.id;
      chips.forEach((x, i) => x.classList.toggle('on', SOURCES[i].id === source));
      tick();
    });
    return b;
  });
  const status = h('span', { class: 'dim', style: { fontSize: '0.84em' } });
  const canvas = h('canvas', { class: 'spectrum' });
  c.right.append(...chips);
  c.body.append(canvas, h('div', { class: 'card-note' }, status));

  let timer = null;
  let busy = false;
  let last = null;

  function draw(mag, centerHz, spanHz, marks) {
    const dpr = window.devicePixelRatio || 1;
    const W = canvas.clientWidth || 600;
    const H = canvas.clientHeight || 240;
    if (canvas.width !== Math.round(W * dpr)) { canvas.width = Math.round(W * dpr); canvas.height = Math.round(H * dpr); }
    const g = canvas.getContext('2d');
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, W, H);
    if (!mag || !mag.length) return;
    let lo = Infinity, hi = -Infinity;
    for (const v of mag) { if (v < lo) lo = v; if (v > hi) hi = v; }
    lo -= 3; hi += 3;
    const PL = 40, PB = 20;
    const pw = W - PL - 6, ph = H - PB - 6;
    const x = i => PL + (i / (mag.length - 1)) * pw;
    const y = v => 6 + ph - ((v - lo) / (hi - lo)) * ph;
    g.font = '11px ' + css('--mono');
    g.fillStyle = css('--text-faint');
    g.strokeStyle = css('--line');
    g.lineWidth = 1;
    const step = (hi - lo) > 60 ? 20 : 10;
    for (let v = Math.ceil(lo / step) * step; v <= hi; v += step) {
      g.beginPath(); g.moveTo(PL, y(v)); g.lineTo(PL + pw, y(v)); g.stroke();
      g.fillText(v.toFixed(0), 4, y(v) + 4);
    }
    const f0 = centerHz - spanHz / 2;
    for (const m of marks) {
      const fx = PL + ((m.hz - f0) / spanHz) * pw;
      if (fx < PL || fx > PL + pw) continue;
      g.strokeStyle = m.color;
      g.setLineDash([4, 4]);
      g.beginPath(); g.moveTo(fx, 6); g.lineTo(fx, 6 + ph); g.stroke();
      g.setLineDash([]);
      g.fillStyle = m.color;
      g.fillText(m.label, Math.min(fx + 4, W - 60), 18);
    }
    g.strokeStyle = css('--accent');
    g.beginPath();
    for (let i = 0; i < mag.length; i++) {
      if (i === 0) g.moveTo(x(i), y(mag[i])); else g.lineTo(x(i), y(mag[i]));
    }
    g.stroke();
    g.fillStyle = css('--text-faint');
    g.fillText(((centerHz - spanHz / 2) / 1e6).toFixed(3), PL, H - 4);
    const r = ((centerHz + spanHz / 2) / 1e6).toFixed(3) + ' MHz';
    g.fillText(r, PL + pw - g.measureText(r).width, H - 4);
  }

  function marks() {
    const s = store.state;
    const out = [];
    if (!s) return out;
    if (s.site.cc_freq_hz) out.push({ hz: s.site.cc_freq_hz, label: 'CC', color: css('--acquire') });
    const t = s.call ? s.call.freq_hz : s.chain.parked_freq_hz;
    if (t) out.push({ hz: t, label: s.call ? 'TG ' + s.call.tg : 'parked', color: s.call ? css('--voice') : css('--idle') });
    return out;
  }

  async function tick() {
    if (busy || document.hidden) return;
    busy = true;
    const src = source;
    try {
      if (src === 'wide') {
        const r = await api.spectrumWide();
        if (src !== source) return;
        if (!r.ok) throw new Error(r.error || 'no data');
        last = [r.mag_db, r.center_hz, r.span_hz, marks()];
        setText(status, r.bins + ' bins · ' + (r.span_hz / 1e6).toFixed(2) + ' MHz span (HDL FFT)');
      } else {
        const r = await api.spectrum(src, 4096, 2);
        if (src !== source) return;
        if (!r.ok) throw new Error(r.error || 'no data');
        last = [r.mag_db, r.center_hz, r.sample_rate_hz, []];
        setText(status, src + ' DDC · ' + r.fft_size + '-pt ×' + r.averages_used + ' · '
          + (r.sample_rate_hz / 1e3).toFixed(1) + ' kHz span, centre ' + (r.center_hz / 1e6).toFixed(4) + ' MHz');
      }
      draw(...last);
    } catch (e) {
      setText(status, 'spectrum unavailable: ' + e.message);
    } finally {
      busy = false;
    }
  }

  const onResize = () => { if (last) draw(...last); };
  window.addEventListener('resize', onResize);
  timer = setInterval(tick, POLL_MS);
  tick();
  return {
    el: c.el,
    unmount: () => { clearInterval(timer); window.removeEventListener('resize', onResize); },
  };
}
