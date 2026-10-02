// The spectrum card (Diagnostics): the receive window from the wideband spectrometer, with the
// control channel and the lanes marked; each frame the radio pushes while the card is open.

import { h, card, setText } from '../dom.js';
import { mhz } from '../format.js';
import { want } from '../store.js';

const BINS = 1024;

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || '#888';
}

export function spectrumCard() {
  const c = card('Spectrum');
  const canvas = h('canvas', { style: { width: '100%', height: '200px', display: 'block' }, 'aria-label': 'Spectrum of the receive window' });
  const note = h('p', { class: 'card-note', text: 'Waiting for a frame (none comes while a scan has the radio).' });
  c.body.append(canvas, note);
  let data = null;

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

  const stop = want({ spectrum: BINS }, m => {
    if (!m.spectrum.db.length) return;
    data = m.spectrum;
    draw();
    setText(note, `LO ${mhz(data.lo_hz)}, ${(data.sample_rate_hz / 1e6).toFixed(0)} MSPS; control channel ${mhz(data.control_hz)}`);
  });

  window.addEventListener('resize', draw);
  return {
    el: c.el,
    stop() {
      stop();
      window.removeEventListener('resize', draw);
    },
  };
}
