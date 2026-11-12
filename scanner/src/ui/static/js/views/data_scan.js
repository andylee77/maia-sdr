// Data's Scan page, laid out as the module was drawn (the canvas's A3): the windows in a strip
// with what the mode is asked to do, the window as a map of its bursts (newest at the top), and
// the emitters heard in it. One window is built, parked at 433.92 MHz, and where the drawing has
// Park, Next window and Stop the strip has "Keep IQ": the next bursts' IQ kept for Captures.
// An emitter a decoder has read is named as its device, and opens on Devices. The radio pushes
// what it hears at each change. What is shown stays while another module has
// the radio: it is what was last heard.

import { h, card, toast, setText, setClass, keyedList, icon } from '../dom.js';
import { api } from '../api.js';
import { want, boardNow } from '../store.js';
import { mhz, dur, ago, num, clock, DASH } from '../format.js';
import { deviceName, readingText } from './data_parts.js';

// The map shows this long: the slowest device of unit A's first look sends every 53 s.
const MAP_MS = 180000;
// The map's frequency marks, Hz apart.
const TICK_HZ = 20000;
const plural = (n, one) => `${num(n)} ${one}${n === 1 ? '' : 's'}`;
// Three decimals: a burst's centre is read to about a kilohertz.
const at = hz => (hz / 1e6).toFixed(3);
// An emitter is the bursts within 3 kHz: its own next bursts are asked for that near it.
const NEAR_HZ = 6000;

function stateLine(v) {
  if (v.state === 'error') return `Not listening: ${v.error}`;
  if (v.state === 'starting') return 'Starting…';
  const w = v.window;
  if (v.state !== 'listening') return w ? 'Not listening: another module has the radio. This is what was last heard.' : 'Not listening: another module has the radio. Nothing was heard since the scanner started.';
  const how = w.source === 'replay' ? 'a replayed capture in the radio\'s place' : `${w.gain_db} dB of gain`;
  return `Parked: the one window so far. A ${w.sample_rate_hz / 1e6} MSPS window at ${mhz(w.lo_hz, 2)}, ${how}.`;
}

function countsLine(v) {
  const c = v.counts;
  const parts = [`${plural(c.bursts, 'burst')} since the scanner started`];
  if (v.floor_dbfs !== null) parts.push(`noise floor ${v.floor_dbfs.toFixed(1)} dBFS`);
  if (c.clicks) parts.push(`${num(c.clicks)} ms of broadband clicks left out`);
  if (c.gaps || c.dropped) parts.push(`${plural(c.gaps + c.dropped, 'gap')} in the samples`);
  if (c.capture_errors) parts.push(`${plural(c.capture_errors, 'capture')} not kept`);
  return parts.join(' · ');
}

export function mount(el) {
  let view = null;
  let gone = false;

  // ── The windows, and what the mode is asked to do ──
  const chip = h('span', { class: 'data-win' }, h('strong', { text: '433' }), h('span', { text: 'parked' }));
  const state = h('span', { class: 'data-state', 'aria-live': 'polite' });
  const count = h('input', { class: 'input num', type: 'number', min: 1, max: 100, step: 1, value: 5, 'aria-label': 'Bursts to keep' });
  const keepBtn = h('button', { class: 'btn', type: 'button', title: 'Keep the IQ of the next bursts, whatever their frequency, for Captures' }, icon('download', 16), h('span', { text: 'Keep IQ' }));
  const stopBtn = h('button', { class: 'btn', type: 'button', text: 'Stop keeping', hidden: true });
  const armed = h('span', { class: 'data-armed dim', 'aria-live': 'polite' });
  const strip = h('section', { class: 'card data-strip' },
    h('div', { class: 'data-wins' }, chip, state),
    h('div', { class: 'data-keep' }, armed, h('label', { class: 'data-next' }, h('span', { class: 'dim', text: 'Next' }), count, h('span', { class: 'dim', text: 'bursts' })), keepBtn, stopBtn));

  async function arm(n, freqHz) {
    if (!Number.isInteger(n)) return toast('The bursts to keep are a whole number.', true);
    try {
      view = await api.dataArm(n, freqHz, NEAR_HZ);
      if (!gone) draw();
    } catch (e) {
      toast(e.message, true);
    }
    return null;
  }
  keepBtn.addEventListener('click', () => arm(Number(count.value), null));
  stopBtn.addEventListener('click', async () => {
    try {
      view = await api.dataDisarm();
      if (!gone) draw();
    } catch (e) {
      toast(e.message, true);
    }
  });

  // ── The window: its bursts by frequency, newest at the top ──
  const span = h('strong', { class: 'data-span' });
  const field = h('div', { class: 'data-field' });
  const spot = h('div', { class: 'data-spot' });
  const marks = h('div', { class: 'data-marks' });
  const counts = h('p', { class: 'card-note data-counts' });
  const map = h('section', { class: 'card data-map' },
    h('div', { class: 'data-map-head' }, span,
      h('span', { class: 'data-key' }, h('i', { class: 'burst' }), 'Burst'), h('span', { class: 'data-key' }, h('i', { class: 'kept' }), 'IQ kept'),
      h('span', { class: 'spacer' }), h('span', { class: 'dim', text: `Newest at the top · ${MAP_MS / 60000} minutes` })),
    h('div', { class: 'data-plot' }, spot, field), marks, counts);

  const where = hz => (view && view.window ? (hz - view.window.low_hz) / (view.window.high_hz - view.window.low_hz) * 100 : 0);
  function place(dash, b) {
    const w = view.window;
    const wide = Math.max(1.2, b.width_hz / (w.high_hz - w.low_hz) * 100);
    dash.style.left = `${Math.min(100 - wide, Math.max(0, where(b.freq_hz) - wide / 2))}%`;
    dash.style.width = `${wide}%`;
    dash.style.top = `${(boardNow() - b.unix_ms) / MAP_MS * 100}%`;
    dash.title = `${clock(b.unix_ms, true)} · ${at(b.freq_hz)} MHz · ${num(b.length_ms)} ms (${num(b.on_ms)} ms on) · ${b.over_db.toFixed(1)} dB over the floor${b.reading ? ` · ${b.reading.model}: ${readingText(b.reading)}` : ''}${b.capture ? ' · IQ kept' : ''}`;
    setClass(dash, 'kept', !!b.capture);
  }
  // The bursts young enough for the map.
  const shown = () => (view && view.window ? view.bursts.filter(b => boardNow() - b.unix_ms < MAP_MS) : []);

  // ── The emitters heard in the window ──
  const heard = card('Heard in this window');
  const heardCount = h('span', { class: 'dim' });
  heard.head.querySelector('h2').after(heardCount);
  const emBody = h('tbody');
  const emNone = h('p', { class: 'dim', text: 'None heard yet.' });
  heard.body.append(
    h('div', { class: 'table-wrap data-table' }, h('table', { class: 'table' },
      h('thead', null, h('tr', null, ...['MHz', 'Device', 'Bursts', 'Every', 'Over the floor', 'Signature', 'Last heard', ''].map(t => h('th', { text: t })))), emBody)),
    emNone,
    h('p', { class: 'card-note', text: 'An emitter is the bursts of one frequency (within 3 kHz); a burst is one transmission, a message with its repeats. Every, the level and the signature are the medians of its last 16 bursts. One whose bursts a decoder reads is named as its device; none can be named by hand yet.' }));

  function emitterRow() {
    const btn = h('button', { class: 'btn small', type: 'button', title: 'Keep the IQ of this emitter\'s next bursts (as many as Next says)' }, icon('download', 14), h('span', { text: 'Keep' }));
    btn.addEventListener('click', () => arm(Number(count.value), Number(btn.dataset.hz)));
    const open = h('a', { class: 'btn small', href: '#devices', title: 'Its readings, on Devices', text: 'Open', hidden: true });
    return h('tr', null, h('td', { class: 'num data-mhz' }), h('td'), ...Array.from({ length: 5 }, () => h('td', { class: 'num' })), h('td', { class: 'files-get data-acts' }, open, btn));
  }
  function fillEmitter(tr, e) {
    const c = tr.children;
    const device = e.device && view.devices.find(d => d.key === e.device);
    setText(c[0], at(e.freq_hz));
    setText(c[1], device ? deviceName(device) : 'Not identified');
    setClass(c[1], 'dim', !device);
    setText(c[2], num(e.bursts));
    setText(c[3], e.every_ms === null ? DASH : dur(e.every_ms));
    setText(c[4], `${e.over_db.toFixed(1)} dB`);
    c[4].title = `${e.level_dbfs.toFixed(1)} dBFS at its strongest millisecond`;
    setText(c[5], `${num(e.length_ms)} ms, ${(e.width_hz / 1e3).toFixed(1)} kHz wide${device ? ', decoded' : ''}`);
    setText(c[6], ago(boardNow() - e.last_unix_ms));
    c[7].firstChild.hidden = !device;
    const btn = c[7].lastChild;
    btn.dataset.hz = String(e.freq_hz);
    btn.disabled = view.state !== 'listening' || !view.captures;
  }

  el.append(h('div', { class: 'data-page' }, strip, map, heard.el));

  // The frequency marks under the map, once its window is known.
  let marked = null;
  function mark(w) {
    if (marked === `${w.low_hz}-${w.high_hz}`) return;
    marked = `${w.low_hz}-${w.high_hz}`;
    const ticks = [];
    for (let hz = Math.ceil(w.low_hz / TICK_HZ) * TICK_HZ; hz <= w.high_hz; hz += TICK_HZ) ticks.push(hz);
    marks.replaceChildren(...ticks.map(hz => h('span', { style: { left: `${where(hz)}%` }, text: (hz / 1e6).toFixed(2) })));
    spot.style.left = `${where(w.spot_hz)}%`;
    spot.title = `${mhz(w.spot_hz, 2)}: the frequency listened around`;
  }

  function draw() {
    if (!view) return;
    const v = view;
    const listening = v.state === 'listening';
    setClass(chip, 'on', listening);
    setText(state, stateLine(v));
    setClass(state, 'warn', v.state === 'error');
    setText(span, v.window ? `${at(v.window.low_hz)} to ${at(v.window.high_hz)} MHz` : 'The window');
    setText(counts, countsLine(v));
    keepBtn.disabled = count.disabled = !listening || !v.captures;
    stopBtn.hidden = !v.arm;
    setText(armed, !v.captures ? 'This radio has nowhere to keep captures.'
      : v.arm ? `Keeping the next ${plural(v.arm.left, 'burst')}${v.arm.freq_hz ? ` within ${v.arm.span_hz / 2e3} kHz of ${at(v.arm.freq_hz)} MHz` : ''}.` : '');
    if (v.window) mark(v.window);
    keyedList(field, shown(), b => b.seq, () => h('i', { class: 'data-dash' }), place);
    setText(heardCount, v.emitters.length ? plural(v.emitters.length, 'emitter') : '');
    emNone.hidden = v.emitters.length > 0;
    keyedList(emBody, v.emitters, e => e.id, emitterRow, fillEmitter);
  }

  const stop = want({ data: true }, m => {
    view = m.data;
    draw();
  });
  api.data().then(v => {
    if (gone || (view && view.rev >= v.rev)) return;
    view = v;
    draw();
  }).catch(e => { if (!gone) toast(e.message, true); });

  return {
    // The map moves down and the ages on with the board's clock.
    update() {
      if (!view) return;
      keyedList(field, shown(), b => b.seq, () => h('i', { class: 'data-dash' }), place);
      for (const tr of emBody.children) {
        const e = view.emitters.find(x => String(x.id) === tr.dataset.key);
        if (e) setText(tr.children[6], ago(boardNow() - e.last_unix_ms));
      }
    },
    unmount() {
      gone = true;
      stop();
    },
  };
}
