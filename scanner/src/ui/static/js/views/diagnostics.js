// Diagnostics: the event log, the spectrum of the window, the live site's receive window, its
// control channel's counters, the tuning, the hardware and the API's routes; the radio pushes
// each while the page is open.

import { h, card, table, toast, switchInput } from '../dom.js';
import { mhz, uptime, clockMs, ago, num, pct, DASH } from '../format.js';
import { protocol } from '../protocols.js';
import { api } from '../api.js';
import { want } from '../store.js';
import { spectrumCard } from './spectrum.js';
import { boardCard } from './board.js';
import { windowCard } from './window.js';

// Lines kept in the events box.
const KEEP = 500;

export function mount(el) {
  const events = card('Events');
  const control = card('Control channel');
  const tuning = card('Tuning');
  const routes = card('API routes');
  const spectrum = spectrumCard();
  const receive = windowCard();
  const board = boardCard();
  el.append(h('div', { class: 'stack' }, events.el, spectrum.el, receive.el, control.el, tuning.el, board.el, routes.el));
  const kv = rows => h('table', { class: 'kv' }, ...rows.map(([k, v]) => h('tr', null, h('th', { text: k }), h('td', { text: v }))));

  const log = h('div', { class: 'log', role: 'log' });
  events.body.append(log);

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
  const housekeeping = switchInput('Housekeeping', false, on => {
    stopEvents();
    wants.events = { after: 0, routine: on };
    log.replaceChildren();
    stopEvents = want(wants, lines);
  });
  events.right.append(housekeeping.el);

  api.routes()
    .then(list => routes.body.append(table(['Method', 'Path', ''], list.map(r =>
      h('tr', null, h('td', { text: r.method.toUpperCase() }), h('td', { text: r.path }), h('td', { class: 'dim', text: r.summary }))))))
    .catch(e => toast(e.message, true));

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
      tuning.body.replaceChildren(kv(rows));
      // The live site's control channel: how well it decodes, by its protocol's counters.
      const live = st.live && st.live.state === 'live' ? st.live : null;
      const c = st.control || {};
      const p = live ? protocol(live.system.protocol) : null;
      control.body.replaceChildren(live ? kv([
        ['Site', `${live.site.label} · ${mhz(live.site.control.freq_hz)}`],
        ['Messages', c.msgs_per_s === null || c.msgs_per_s === undefined ? DASH : `${num(c.msgs_per_s, 1)} /s`],
        [p.okLabel, pct(c.ok_pct)],
        ['Last message', ago(c.last_message_age_ms)],
        ...p.healthRows(c),
        ['Grants', num(c.grants)],
        ['Window', `${t.preset || DASH} at ${mhz(t.lo_hz)}`],
        ['CPU', pct(c.cpu_pct, 0)],
      ]) : h('p', { class: 'dim', text: 'No site is live.' }));
    },
    unmount() {
      stopEvents();
      spectrum.stop();
      receive.stop();
      board.stop();
    },
  };
}
