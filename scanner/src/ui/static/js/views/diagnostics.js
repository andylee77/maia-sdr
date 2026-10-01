// Diagnostics: the tuning, the hardware and the API's routes.

import { h, card, table, toast } from '../dom.js';
import { mhz, uptime, DASH } from '../format.js';
import { api } from '../api.js';

export function mount(el) {
  const tuning = card('Tuning');
  const routes = card('API routes');
  el.append(h('div', { class: 'stack' }, tuning.el, routes.el));

  api.routes()
    .then(list => routes.body.append(table(['Method', 'Path', ''], list.map(r =>
      h('tr', null, h('td', { text: r.method.toUpperCase() }), h('td', { text: r.path }), h('td', { class: 'dim', text: r.summary }))))))
    .catch(e => toast(e.message, true));

  return {
    update(s) {
      const st = s.status;
      if (!st) return;
      const t = st.tuning;
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
      ];
      tuning.body.replaceChildren(h('table', { class: 'kv' }, ...rows.map(([k, v]) =>
        h('tr', null, h('th', { text: k }), h('td', { text: v }))));
    },
    unmount() {},
  };
}
