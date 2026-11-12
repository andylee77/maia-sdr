// Data's Devices page, laid out as the study drew it: every device the decoders have read, each
// a row with what it last said; under them the picked device's data log, the last things it
// said, with its temperature drawn as a line. The radio pushes the devices as they change, and
// the log is read again when its device says something new. The log is the radio's memory since
// it started: the data log on the card, the names and the filters are not built.

import { h, card, toast, setText, setClass, keyedList, icon } from '../dom.js';
import { api } from '../api.js';
import { want, boardNow, store } from '../store.js';
import { dur, ago, num, clock, temperature, DASH } from '../format.js';
import { deviceName, readingText } from './data_parts.js';

const KIND = { weather: 'Weather' };
const at = hz => (hz / 1e6).toFixed(3);
const plural = (n, one) => `${num(n)} ${one}${n === 1 ? '' : 's'}`;
// An emitter is the bursts within 3 kHz: a device's next burst is asked for that near it.
const NEAR_HZ = 6000;

// A line through `values` (oldest first), 200 by 28.
function sparkline(values) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 200 28');
  svg.setAttribute('class', 'data-spark');
  svg.setAttribute('aria-hidden', 'true');
  if (values.length > 1) {
    const [low, high] = [Math.min(...values), Math.max(...values)];
    const y = v => (high === low ? 14 : 25 - (v - low) / (high - low) * 22);
    const line = document.createElementNS('http://www.w3.org/2000/svg', 'polyline');
    line.setAttribute('points', values.map((v, k) => `${(k / (values.length - 1) * 200).toFixed(1)},${y(v).toFixed(1)}`).join(' '));
    svg.appendChild(line);
  }
  return svg;
}

export function mount(el) {
  let view = null;
  let picked = null;
  // The picked device's log, and how many messages it had when it was read.
  let log = null;
  let logOf = null;
  let gone = false;
  let unit = store.units.temperature;

  // ── The devices ──
  const list = card('Devices');
  const count = h('span', { class: 'dim' });
  list.head.querySelector('h2').after(count);
  const body = h('tbody');
  const none = h('p', { class: 'dim', text: 'No device read yet. The radio reads Oregon Scientific version 1 temperature sensors so far; everything else it hears is on Scan, by frequency.' });
  list.body.append(
    h('div', { class: 'table-wrap data-table' }, h('table', { class: 'table data-pick' },
      h('thead', null, h('tr', null, ...['Kind', 'Device', 'ID', 'MHz', 'Last', 'Every', 'Over the floor', 'Latest'].map(t => h('th', { text: t })))), body)),
    none);

  function row() {
    const tr = h('tr', { tabindex: 0 }, ...Array.from({ length: 8 }, (_, k) => h('td', { class: k > 1 ? 'num' : null })));
    const pick = () => {
      picked = tr.dataset.key;
      draw();
      readLog();
    };
    tr.addEventListener('click', pick);
    tr.addEventListener('keydown', e => { if (e.key === 'Enter') pick(); });
    return tr;
  }
  function fill(tr, d) {
    const c = tr.children;
    setText(c[0], KIND[d.kind] || d.kind);
    setText(c[1], d.model);
    setText(c[2], d.channel === null ? String(d.id) : `${d.id}, channel ${d.channel}`);
    setText(c[3], at(d.freq_hz));
    setText(c[4], ago(boardNow() - d.last_unix_ms));
    setText(c[5], d.every_ms === null ? DASH : dur(d.every_ms));
    setText(c[6], `${d.over_db.toFixed(1)} dB`);
    setText(c[7], readingText(d.latest));
    setClass(tr, 'sel', d.key === picked);
  }

  // ── The picked device's data log ──
  const one = card('Data log');
  const title = one.head.querySelector('h2');
  const keep = h('button', { class: 'btn small', type: 'button', title: 'Keep the IQ of its next burst, for Captures' }, icon('download', 14), h('span', { text: 'Keep next' }));
  one.right.append(keep);
  const line = h('div', { class: 'data-line' });
  const entries = h('tbody');
  one.body.append(
    line,
    h('div', { class: 'table-wrap data-table' }, h('table', { class: 'table' },
      h('thead', null, h('tr', null, ...['Time', 'MHz', 'Over the floor', 'Level', 'Reading', 'IQ'].map(t => h('th', { text: t })))), entries)),
    h('p', { class: 'card-note', text: 'The last 200 things it said since the radio started, the newest first; they arrive as they are read. A message sent more than once in a burst is one entry.' }));
  one.el.hidden = true;
  keep.addEventListener('click', async () => {
    const d = view && view.devices.find(x => x.key === picked);
    if (!d) return;
    try {
      view = await api.dataArm(1, d.freq_hz, NEAR_HZ);
      toast(`Keeping its next burst, within ${NEAR_HZ / 2e3} kHz of ${at(d.freq_hz)} MHz`);
      if (!gone) draw();
    } catch (e) {
      toast(e.message, true);
    }
  });

  function drawLog() {
    const d = view && view.devices.find(x => x.key === picked);
    one.el.hidden = !d || !log;
    if (!d || !log) return;
    setText(title, `${deviceName(d)}: data log`);
    keep.disabled = view.state !== 'listening' || !view.captures;
    const temps = log.map(s => s.reading.temperature_c).filter(t => t !== null).reverse();
    line.replaceChildren(...(temps.length ? [
      h('span', { class: 'dim', text: 'Temperature' }), sparkline(temps),
      h('strong', { text: temperature(temps[temps.length - 1], unit) }),
      h('span', { class: 'dim', text: temps.length < 2 ? '' : Math.min(...temps) === Math.max(...temps) ? `the same over ${plural(temps.length, 'reading')}`
        : `${temperature(Math.min(...temps), unit)} to ${temperature(Math.max(...temps), unit)} over ${plural(temps.length, 'reading')}` }),
    ] : []));
    keyedList(entries, log, s => s.seq, () => h('tr', null, ...Array.from({ length: 6 }, () => h('td', { class: 'num' }))), (tr, s) => {
      const c = tr.children;
      setText(c[0], clock(s.unix_ms, true));
      setText(c[1], at(s.freq_hz));
      setText(c[2], `${s.over_db.toFixed(1)} dB`);
      setText(c[3], `${s.level_dbfs.toFixed(1)} dBFS`);
      setText(c[4], `${readingText(s.reading)}${s.reading.repeats > 1 ? ` · sent ${s.reading.repeats} times` : ''}`);
      if (c[5].dataset.capture !== (s.capture || '')) {
        c[5].dataset.capture = s.capture || '';
        c[5].replaceChildren(...(s.capture ? [h('a', { href: '#captures', title: `Kept as ${s.capture}`, text: 'kept' })] : []));
      }
    });
  }

  async function readLog() {
    const key = picked;
    const d = view && view.devices.find(x => x.key === key);
    if (!d) return;
    logOf = `${key}:${d.messages}:${view.counts.captures}`;
    try {
      const got = await api.dataDevice(key);
      if (gone || picked !== key) return;
      log = got.log;
      drawLog();
    } catch (e) {
      if (!gone) toast(e.message, true);
    }
  }

  el.append(h('div', { class: 'data-page' }, list.el, one.el));

  function draw() {
    if (!view) return;
    const devices = view.devices;
    if (!devices.some(d => d.key === picked)) {
      picked = devices.length ? devices[0].key : null;
      log = null;
    }
    setText(count, devices.length ? plural(devices.length, 'device') : '');
    none.hidden = devices.length > 0;
    keyedList(body, devices, d => d.key, row, fill);
    drawLog();
    // The picked device said something new, or a capture of it was written: its log again.
    const d = devices.find(x => x.key === picked);
    if (d && logOf !== `${picked}:${d.messages}:${view.counts.captures}`) readLog();
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
    // The ages move on with the board's clock; the unit follows the radio's Settings.
    update(s) {
      if (!view) return;
      if (s.units.temperature !== unit) {
        unit = s.units.temperature;
        draw();
        return;
      }
      for (const tr of body.children) {
        const d = view.devices.find(x => x.key === tr.dataset.key);
        if (d) setText(tr.children[4], ago(boardNow() - d.last_unix_ms));
      }
    },
    unmount() {
      gone = true;
      stop();
    },
  };
}
