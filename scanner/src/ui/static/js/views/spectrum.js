// The spectrum card (Diagnostics): the receive window from the wideband spectrometer, with the
// control channel and the lanes marked. Refreshed while the page is open.

import { h, card, setText } from '../dom.js';
import { mhz } from '../format.js';
import { api } from '../api.js';

const EVERY_MS = 1000;

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || '#888';
}

export function spectrumCard() {
  const c = card('Spectrum');
  const canvas = h('canvas', { style: { width: '100%', height: '200px', display: 'block' }, 'aria-label': 'Spectrum of the receive window' });
  const note = h('p', { class: 'card-note', text: ' ' });
  c.body.append(canvas, note);
  let data = null;
  let timer = null;

  function draw() {
    if (!data || !data.db.length) return;
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth || 600;
    const ht = canvas.clientHeight || 200;
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(ht * dpr);
    const g = canvas.getContext('2d');
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, w, ht);
    const sorted = [...data.db].sort((a, b) => a - b);
    const floor = sorted[Math.floor(sorted.length * 0.1)];
    const top = Math.max(floor + 30, sorted[sorted.length - 1] + 3);
    const left = 4, bottom = 18;
    const ph = ht - bottom;
    const x = hz => left + ((hz - data.lo_hz) / data.sample_rate_hz + 0.5) * (w - left * 2);
    const y = db => ph - ((db - floor + 3) / (top - floor + 3)) * ph;
    // Markers: the control channel, then the lanes.
    const marks = [[data.control_hz, cssVar('--accent')], ...data.lanes_hz.filter(Boolean).map(f => [f, cssVar('--voice')])];
    for (const [f, color] of marks) {
      g.fillStyle = color;
      g.globalAlpha = 0.25;
      g.fillRect(x(f) - 2, 0, 4, ph);
    }
    g.globalAlpha = 1;
    g.strokeStyle = cssVar('--text-dim');
    g.lineWidth = 1;
    g.beginPath();
    data.db.forEach((v, i) => {
      const px = left + (i / (data.db.length - 1)) * (w - left * 2);
      if (i === 0) g.moveTo(px, y(v));
      else g.lineTo(px, y(v));
    });
    g.stroke();
    g.fillStyle = cssVar('--text-faint');
    g.font = '11px system-ui, sans-serif';
    g.textAlign = 'center';
    g.textBaseline = 'top';
    for (let k = -2; k <= 2; k++) {
      const f = data.lo_hz + (k / 5) * data.sample_rate_hz;
      g.fillText((f / 1e6).toFixed(2), x(f), ph + 4);
    }
  }

  async function poll() {
    try {
      data = await api.spectrum();
      if (data.db.length) {
        draw();
        setText(note, `LO ${mhz(data.lo_hz)}, ${(data.sample_rate_hz / 1e6).toFixed(0)} MSPS; control channel ${mhz(data.control_hz)}`
          + (data.fresh ? '' : ' (the last frame: a scan has the radio)'));
      } else {
        setText(note, 'No spectrum yet.');
      }
    } catch (e) {
      setText(note, e.message);
    }
    timer = setTimeout(poll, EVERY_MS);
  }

  window.addEventListener('resize', draw);
  poll();
  return { el: c.el, stop() { clearTimeout(timer); window.removeEventListener('resize', draw); } };
}
