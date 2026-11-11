// Diagnostics, filling the screen: the spectrum of the receive window across the top; under it
// the event log (every protocol's messages and the radio's own), with the carriers heard in the
// window and the API's routes behind the same tabs; and down the right the live site's control
// channel, its receive window, the tuning and the hardware. The radio pushes each while the page
// is open.

import { h, card, table, toast, setClass, switchInput } from '../dom.js';
import { mhz, uptime, clockMs, ago, num, pct, DASH } from '../format.js';
import { protocol } from '../protocols.js';
import { api } from '../api.js';
import { want } from '../store.js';
import { spectrumCard } from './spectrum.js';
import { boardCard } from './board.js';
import { windowCard } from './window.js';

// Lines kept in the events box.
const KEEP = 500;
const TABS = [['events', 'Events'], ['heard', 'Carriers'], ['routes', 'API routes']];

// Kept while the page is left for another.
let tab = 'events';

const kv = rows => h('dl', { class: 'kv' }, ...rows.flatMap(([k, v]) => [h('dt', { text: k }), h('dd', { text: v })]));

export function mount(el) {
  const control = card('Control channel');
  const tuning = card('Tuning');
  const spectrum = spectrumCard();
  const receive = windowCard();
  const board = boardCard();

  // The main pane: the events, the carriers heard, or the routes.
  const log = h('div', { class: 'log', role: 'log' });
  const routes = h('div', { class: 'diag-pad', hidden: true }, h('p', { class: 'dim', text: 'Reading the routes…' }));
  receive.heard.hidden = true;
  const panes = { events: log, heard: receive.heard, routes };
  const housekeeping = switchInput('Housekeeping', false, on => {
    stopEvents();
    wants.events = { after: 0, routine: on };
    log.replaceChildren();
    stopEvents = want(wants, lines);
  });
  const chips = TABS.map(([value, text]) => h('button', { class: 'chip', type: 'button', text, onclick: () => {
    tab = value;
    showTab();
  } }));
  const main = h('section', { class: 'card flush diag-main', 'aria-label': 'Events, carriers and routes' },
    h('div', { class: 'card-head' }, h('div', { class: 'row', role: 'group', 'aria-label': 'What to show' }, ...chips), h('div', { class: 'spacer' }), housekeeping.el),
    h('div', { class: 'card-body' }, log, receive.heard, routes));
  function showTab() {
    chips.forEach((chip, i) => {
      setClass(chip, 'on', TABS[i][0] === tab);
      chip.setAttribute('aria-pressed', String(TABS[i][0] === tab));
    });
    for (const [key, pane] of Object.entries(panes)) pane.hidden = key !== tab;
    housekeeping.el.hidden = tab !== 'events';
    // A log shown again is read from its end.
    if (tab === 'events') log.scrollTop = log.scrollHeight;
  }

  el.append(h('div', { class: 'diag' }, spectrum.el, main,
    h('div', { class: 'diag-side' }, control.el, receive.el, tuning.el, board.el)));

  function line(e) {
    return h('div', { class: 'log-line' + (e.valid ? '' : ' dim') },
      h('span', { class: 't', text: clockMs(e.unix_ms) }),
      h('span', { class: 'c c-' + (e.source === 'system' ? 'system' : 'cc'), text: e.source }),
      h('span', { class: 'm', text: e.text }));
  }

  // The log's new lines as the radio writes them; `after` moves on so a resubscription does not
  // send them again.
  function lines(m) {
    if (m.type === 'restart') {
      log.replaceChildren();
      return;
    }
    const list = m.events;
    if (!list.length) return;
    const pinned = log.scrollTop + log.clientHeight >= log.scrollHeight - 4;
    for (const e of list) log.append(line(e));
    wants.events.after = list[list.length - 1].seq;
    while (log.childElementCount > KEEP) log.firstElementChild.remove();
    if (pinned) log.scrollTop = log.scrollHeight;
  }
  const wants = { events: { after: 0, routine: false } };
  let stopEvents = want(wants, lines);
  showTab();

  api.routes()
    .then(list => routes.replaceChildren(table(['Method', 'Path', ''], list.map(r =>
      h('tr', null, h('td', { text: r.method.toUpperCase() }), h('td', { class: 'mono', text: r.path }), h('td', { class: 'dim', text: r.summary }))))))
    .catch(e => toast(e.message, true));

  // The two cards drawn from the status are drawn again only when their rows change.
  let drawnTuning = null;
  let drawnControl = null;
  return {
    update(s) {
      receive.update(s);
      const st = s.status;
      if (!st) return;
      const t = st.tuning;
      const i = (st.control && st.control.input) || {};
      const rows = [
        ['Build', st.build],
        ['Up', uptime(st.uptime_s)],
        ['Radio', st.lease],
        ['Preset', t.preset || DASH],
        ['LO (nominal)', mhz(t.lo_hz, 6)],
        ['Crystal', `${t.crystal_ppm.toFixed(3)} ppm, shift ${t.lo_shift_hz} Hz`],
        ['Control channel', mhz(t.control_hz)],
        ['Lane 1', mhz(t.lanes[0])],
        ['Lane 2', mhz(t.lanes[1])],
        ['Control IQ', `${i.blocks ?? DASH} blocks (${i.dropped ?? 0} dropped, ${i.gaps ?? 0} after a gap)`],
      ];
      const tuningKey = JSON.stringify(rows);
      if (tuningKey !== drawnTuning) {
        drawnTuning = tuningKey;
        tuning.body.replaceChildren(kv(rows));
      }
      // The live site's control channel: how well it decodes, by its protocol's counters.
      const live = st.live && st.live.state === 'live' ? st.live : null;
      const c = st.control || {};
      const p = live ? protocol(live.system.protocol) : null;
      const health = live ? [
        ['Site', `${live.site.label} · ${mhz(live.site.control.freq_hz)}`],
        ['Messages', c.msgs_per_s === null || c.msgs_per_s === undefined ? DASH : `${num(c.msgs_per_s, 1)} /s`],
        [p.okLabel, pct(c.ok_pct)],
        ['Last message', ago(c.last_message_age_ms)],
        ...p.healthRows(c),
        ['Grants', num(c.grants)],
        ['Window', `${t.preset || DASH} at ${mhz(t.lo_hz)}`],
        ['CPU', pct(c.cpu_pct, 0)],
      ] : null;
      const controlKey = JSON.stringify(health);
      if (controlKey !== drawnControl) {
        drawnControl = controlKey;
        control.body.replaceChildren(health ? kv(health) : h('p', { class: 'dim', text: 'No site is live.' }));
      }
    },
    unmount() {
      stopEvents();
      spectrum.stop();
      receive.stop();
      board.stop();
    },
  };
}
