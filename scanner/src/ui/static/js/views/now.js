// Now: the live site at a glance, or the way to add one.

import { h, card } from '../dom.js';
import { mhz, DASH } from '../format.js';
import { protocol } from '../protocols.js';

export function mount(el) {
  const c = card('Live site');
  const body = h('div', { class: 'stack' });
  c.body.append(body);
  el.append(c.el);

  return {
    update(s) {
      const live = s.status && s.status.live;
      body.replaceChildren();
      if (!live || live.state === 'no_site') {
        body.append(
          h('p', { text: 'No sites yet.' }),
          h('p', null, h('a', { href: '#systems', text: 'Add a system' }), ' to start receiving.'));
        return;
      }
      if (live.state === 'switching') {
        body.append(h('p', { text: `Switching to ${live.to}…` }));
        return;
      }
      const p = protocol(live.system.protocol);
      const t = live.tuning;
      const rows = [
        ['System', live.system.label],
        ['Protocol', p.label],
        ['Site', live.site.label],
        ['Control channel', mhz(live.site.control.freq_hz)],
        ...p.siteIdentity(live.site.identity),
        ['Window', `${t.preset || DASH} at ${mhz(t.lo_hz)}`],
        ['Profile', live.profile ? live.profile.name : DASH],
      ];
      body.append(h('table', { class: 'kv' }, ...rows.map(([k, v]) =>
        h('tr', null, h('th', { text: k }), h('td', { text: v === null || v === undefined ? DASH : v })))));
    },
    unmount() {},
  };
}
