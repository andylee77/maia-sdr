// Diagnostics: the event log, the spectrum of the window, the tuning, the hardware and the API's
// routes.

import { h, card, table, toast, switchInput } from '../dom.js';
import { mhz, uptime, clockMs, DASH } from '../format.js';
import { api } from '../api.js';
import { spectrumCard } from './spectrum.js';

// Lines kept in the events box.
const KEEP = 500;

export function mount(el) {
  const events = card('Events');
  const tuning = card('Tuning');
  const routes = card('API routes');
  const spectrum = spectrumCard();
  el.append(h('div', { class: 'stack' }, events.el, spectrum.el, tuning.el, routes.el));

  const log = h('div', { class: 'log', role: 'log' });
  events.body.append(log);
  let after = 0;
  let routine = false;
  const housekeeping = switchInput('Housekeeping', false, on => {
    routine = on;
    after = 0;
    log.replaceChildren();
    poll();
  });
  events.right.append(housekeeping.el);

  function line(e) {
    return h('div', { class: 'log-line' + (e.valid ? '' : ' dim') },
      h('span', { class: 't', text: clockMs(e.unix_ms) }),
      h('span', { class: 'c c-' + (e.source === 'system' ? 'system' : 'cc'), text: e.source }),
      h('span', { class: 'm', text: e.text }));
  }

  let busy = false;
  async function poll() {
    if (busy) return;
    busy = true;
    try {
      const { events: list } = await api.events(after, routine);
      if (list.length) {
        const pinned = log.scrollTop + log.clientHeight >= log.scrollHeight - 4;
        for (const e of list) log.append(line(e));
        after = list[list.length - 1].seq;
        while (log.childElementCount > KEEP) log.firstElementChild.remove();
        if (pinned) log.scrollTop = log.scrollHeight;
      }
    } catch (e) {
      toast(e.message, true);
    } finally {
      busy = false;
    }
  }
  poll();
  const timer = setInterval(poll, 2000);

  api.routes()
    .then(list => routes.body.append(table(['Method', 'Path', ''], list.map(r =>
      h('tr', null, h('td', { text: r.method.toUpperCase() }), h('td', { text: r.path }), h('td', { class: 'dim', text: r.summary }))))))
    .catch(e => toast(e.message, true));

  return {
    update(s) {
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
        ['IQ chunks', `${i.iq_chunks ?? DASH} (${i.iq_dropped ?? 0} dropped)`],
        ['Dibit bytes', `${i.dibit_bytes ?? DASH} (${i.dibit_resyncs ?? 0} resyncs, ${i.dibit_lost ?? 0} lost)`],
      ];
      tuning.body.replaceChildren(h('table', { class: 'kv' }, ...rows.map(([k, v]) =>
        h('tr', null, h('th', { text: k }), h('td', { text: v })))));
    },
    unmount() {
      clearInterval(timer);
      spectrum.stop();
    },
  };
}
