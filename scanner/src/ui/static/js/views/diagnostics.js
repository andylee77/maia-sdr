// The radio's own diagnostics (the rail's Diagnostics), filling the screen: what the radio did of
// its own (its start, clock, crystal, each change of mode, the configuration replaced, each
// download of what is on the card), with the API's routes behind the same tabs; and down the
// right the radio itself and the hardware as it reads it back. What one module receives is among
// that module's tabs: the scanner's control channel, spectrum and events, TV's receiver.

import { h, table, toast } from '../dom.js';
import { mhz, uptime } from '../format.js';
import { api } from '../api.js';
import { boardCard } from './board.js';
import { kvCard, mainPane, elsewhere } from './diag_parts.js';

// Kept while the page is left for another.
const state = { tab: 'events' };

export function mount(el) {
  const radio = kvCard('Radio');
  const board = boardCard();
  const routes = h('div', { class: 'diag-pad' }, h('p', { class: 'dim', text: 'Reading the routes…' }));
  const main = mainPane('radio', state, { label: 'The radio\'s events and the API\'s routes', panes: [['routes', 'API routes', routes]] });

  el.append(h('div', { class: 'diag no-plot' }, main.el,
    h('div', { class: 'diag-side' },
      elsewhere('What each module receives is among its own tabs: ', h('a', { href: '#scanner-diag', text: 'Scanner, Diagnostics' }), ' and ', h('a', { href: '#tv-diag', text: 'TV, Diagnostics' }), '.'),
      radio.el, board.el)));

  api.routes()
    .then(list => routes.replaceChildren(table(['Method', 'Path', ''], list.map(r =>
      h('tr', null, h('td', { text: r.method.toUpperCase() }), h('td', { class: 'mono', text: r.path }), h('td', { class: 'dim', text: r.summary }))))))
    .catch(e => toast(e.message, true));

  return {
    update(s) {
      const st = s.status;
      if (!st) return;
      const t = st.tuning;
      radio.show([
        ['Build', st.build],
        ['Up', uptime(st.uptime_s)],
        ['Radio', st.lease],
        ['LO (nominal)', mhz(t.lo_hz, 6)],
        ['Crystal', `${t.crystal_ppm.toFixed(3)} ppm, shift ${t.lo_shift_hz} Hz`],
      ]);
    },
    unmount() {
      main.stop();
      board.stop();
    },
  };
}
