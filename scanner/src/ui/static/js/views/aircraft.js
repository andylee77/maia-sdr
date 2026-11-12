// The Aircraft module's Aircraft page: every aircraft heard in the last minute on either of its
// two links (1090 MHz, Mode S; 978 MHz, UAT), a row each with what it last said: its address,
// the link, its callsign and category, its altitude, speed, track and rate of climb, its
// place, how strong its last message was, how many were heard and when the last. A column's
// head sorts by it. The switch over the table says which link the radio listens to, or both in
// turn: it hears one at a time. The radio pushes the list once a second while it listens; it
// listens only while this module has the radio. The Map tab has them at their places. There
// are no distances: the radio does not know where it is.

import { h, card, toast, setText, setClass, keyedList } from '../dom.js';
import { api } from '../api.js';
import { want, boardNow } from '../store.js';
import { ago, num, DASH } from '../format.js';

const shown = (v, show) => (v === null || v === undefined ? DASH : show(v));
// A rate of climb with its sign, since which way matters more than how fast.
const climb = v => (v === 0 ? '0' : `${v > 0 ? '+' : '−'}${num(Math.abs(v))}`);

// The columns: the head, what a row shows, what it sorts by (text, or a number where none sorts
// last), and whether the numbers run downwards first.
const COLUMNS = [
  { head: 'Address', show: a => a.icao, by: a => a.icao },
  { head: 'Link', show: a => a.link, by: a => a.link },
  { head: 'Callsign', show: a => shown(a.callsign, v => v), by: a => a.callsign },
  { head: 'Category', show: a => shown(a.category, v => v), by: a => a.category },
  { head: 'Altitude', unit: 'ft', show: a => shown(a.altitude_ft, num), by: a => a.altitude_ft, down: true },
  { head: 'Speed', unit: 'kt', show: a => shown(a.speed_kt, v => num(Math.round(v))), by: a => a.speed_kt, down: true },
  { head: 'Track', unit: '°', show: a => shown(a.track_deg, v => String(Math.round(v) % 360).padStart(3, '0')), by: a => a.track_deg },
  { head: 'Climb', unit: 'ft/min', show: a => shown(a.vertical_fpm, climb), by: a => a.vertical_fpm, down: true },
  { head: 'Place', show: a => (a.lat === null ? DASH : `${a.lat.toFixed(4)}, ${a.lon.toFixed(4)}`), by: a => a.lat },
  { head: 'Level', unit: 'dBFS', show: a => a.level_dbfs.toFixed(1), by: a => a.level_dbfs, down: true },
  { head: 'Messages', show: a => num(a.messages), by: a => a.messages, down: true },
  { head: 'Heard', show: a => ago(Math.max(0, boardNow() - a.last_unix_ms)), by: a => a.last_unix_ms, down: true },
];
const TEXT = 4;
// What the radio can be asked to listen to.
const LINKS = [['1090', '1090 MHz', 'Mode S: airliners and most other aircraft'], ['978', '978 MHz', 'UAT: small aircraft in the United States'], ['both', 'Both', '10 s on 1090 MHz, then 5 s on 978 MHz, and so on: the radio hears one at a time']];

function stateLine(v) {
  if (v.state === 'error') return `Not listening: ${v.error}`;
  if (v.state === 'starting') return 'Starting…';
  if (v.state !== 'listening') return 'Not listening: another module has the radio.';
  const w = v.window;
  const how = w.source === 'replay' ? 'a replayed capture in the radio\'s place' : `${w.gain_db} dB of gain`;
  const where = v.link === 'both' ? `1090 and 978 MHz in turn, now ${v.on} MHz` : `${v.on} MHz`;
  return `Listening at ${where}: a ${w.sample_rate_hz / 1e6} MSPS window, ${how}.`;
}

export function mount(el) {
  let view = null;
  let gone = false;
  // The column sorted by, and whether downwards.
  let sort = { column: 0, down: false };

  const list = card('Aircraft');
  const count = h('span', { class: 'dim' });
  list.head.querySelector('h2').after(count);
  const rate = h('span', { class: 'dim aircraft-rate' });
  const links = LINKS.map(([link, text, title]) => {
    const b = h('button', { type: 'button', title, text, 'aria-pressed': 'false' });
    b.addEventListener('click', async () => {
      try {
        view = await api.adsbLink(link);
        if (!gone) draw();
      } catch (e) {
        toast(e.message, true);
      }
    });
    return b;
  });
  list.right.append(rate, h('div', { class: 'seg aircraft-links', role: 'group', 'aria-label': 'The link the radio listens to' }, ...links));
  const heads = COLUMNS.map((c, k) => {
    const b = h('button', { class: 'aircraft-sort', type: 'button', title: `Sort by ${c.head.toLowerCase()}` }, h('span', { text: c.head }), c.unit ? h('span', { class: 'dim', text: ` ${c.unit}` }) : null);
    b.addEventListener('click', () => {
      sort = sort.column === k ? { column: k, down: !sort.down } : { column: k, down: !!c.down };
      draw();
    });
    return h('th', { class: k >= TEXT ? 'num' : null, 'aria-sort': 'none' }, b);
  });
  const body = h('tbody');
  const none = h('p', { class: 'dim' });
  const state = h('p', { class: 'card-note aircraft-state' });
  list.body.append(
    h('div', { class: 'table-wrap aircraft-table' }, h('table', { class: 'table' }, h('thead', null, h('tr', null, ...heads)), body)),
    none, state,
    h('p', { class: 'card-note', text: 'An aircraft is listed once two of its messages were heard, and for a minute after its last. The radio hears one link at a time: with both, each misses what is sent while the other has its turn. Altitudes are by pressure. Not read yet: positions on the ground, squawk codes, and what a ground station sends on 978 MHz.' }));
  el.append(h('div', { class: 'aircraft-page' }, list.el));

  function sorted(aircraft) {
    const by = COLUMNS[sort.column].by;
    const way = sort.down ? -1 : 1;
    return aircraft.slice().sort((a, b) => {
      const [x, y] = [by(a), by(b)];
      // What an aircraft has not said sorts last either way; equals stay by address.
      if (x === null || y === null) return x === y ? a.icao.localeCompare(b.icao) : x === null ? 1 : -1;
      const order = typeof x === 'string' ? x.localeCompare(y) : x - y;
      return order ? way * order : a.icao.localeCompare(b.icao);
    });
  }

  function draw() {
    if (!view) return;
    const aircraft = view.aircraft;
    setText(count, `${num(aircraft.length)} heard in the last minute`);
    setText(rate, view.state === 'listening' ? `${num(view.messages_per_s, 1)} messages a second` : '');
    heads.forEach((th, k) => th.setAttribute('aria-sort', sort.column !== k ? 'none' : sort.down ? 'descending' : 'ascending'));
    links.forEach((b, k) => {
      const on = LINKS[k][0] === view.link;
      setClass(b, 'on', on);
      b.setAttribute('aria-pressed', String(on));
    });
    keyedList(body, sorted(aircraft), a => a.icao,
      () => h('tr', null, ...COLUMNS.map((_, k) => h('td', { class: k >= TEXT ? 'num' : k === 0 ? 'aircraft-icao' : null }))),
      (tr, a) => COLUMNS.forEach((c, k) => setText(tr.children[k], c.show(a))));
    none.hidden = aircraft.length > 0;
    setText(none, view.state === 'listening' ? 'No aircraft heard yet.' : 'No aircraft heard in the last minute.');
    setText(state, stateLine(view));
    setClass(state, 'warn', view.state === 'error');
  }

  const stop = want({ adsb: true }, m => {
    view = m.adsb;
    draw();
  });
  api.adsb().then(v => {
    if (gone || (view && view.rev >= v.rev)) return;
    view = v;
    draw();
  }).catch(e => { if (!gone) toast(e.message, true); });

  return {
    // The ages move on with the radio's clock between two pushes.
    update() { draw(); },
    unmount() {
      gone = true;
      stop();
    },
  };
}
