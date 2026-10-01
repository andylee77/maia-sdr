// Now: the live site at a glance, or the way to add one.

import { h, card } from '../dom.js';
import { mhz, ago, pct, num, DASH } from '../format.js';
import { protocol } from '../protocols.js';

// A control channel with no message for this long is shown as silent.
const SILENT_MS = 5000;

function kv(rows) {
  return h('table', { class: 'kv' }, ...rows.map(([k, v]) =>
    h('tr', null, h('th', { text: k }), h('td', { text: v === null || v === undefined ? DASH : v }))));
}

export function mount(el) {
  const site = card('Live site');
  const control = card('Control channel');
  const dot = h('span', { class: 'dot' });
  control.right.append(dot);
  el.append(h('div', { class: 'stack' }, site.el, control.el));

  return {
    update(s) {
      const live = s.status && s.status.live;
      site.body.replaceChildren();
      control.el.hidden = !live || live.state !== 'live';
      if (!live || live.state === 'no_site') {
        site.body.append(
          h('p', { text: 'No sites yet.' }),
          h('p', null, h('a', { href: '#systems', text: 'Add a system' }), ' to start receiving.'));
        return;
      }
      if (live.state === 'switching') {
        site.body.append(h('p', { text: `Switching to ${live.to}…` }));
        return;
      }
      const p = protocol(live.system.protocol);
      const t = live.tuning;
      site.body.append(kv([
        ['System', live.system.label],
        ['Protocol', p.label],
        ['Site', live.site.label],
        ['Control channel', mhz(live.site.control.freq_hz)],
        ...p.siteIdentity(live.site.identity),
        ['Window', `${t.preset || DASH} at ${mhz(t.lo_hz)}`],
        ['Profile', live.profile ? live.profile.name : DASH],
      ]));

      const c = s.status.control || {};
      const age = c.last_message_age_ms;
      const heard = c.running && age !== null && age !== undefined && age < SILENT_MS;
      dot.className = 'dot' + (heard ? ' ok' : c.running ? ' warn' : '');
      dot.title = heard ? 'decoding' : c.running ? 'no messages' : 'stopped';
      const rows = [
        ...(c.identity ? p.heardIdentity(c.identity) : [['Identity', 'not heard yet']]),
        ['Messages', c.msgs_per_s === null || c.msgs_per_s === undefined ? DASH : `${num(c.msgs_per_s, 1)} /s`],
        [p.okLabel, pct(c.ok_pct)],
        ['Last message', ago(age)],
      ];
      if (c.modulation) rows.push(['Demodulator', c.modulation === 'c4fm' ? 'C4FM (software)' : 'LSM (gateware)']);
      if (c.tsbks_20s) rows.push(['TSBKs in 20 s', `LSM ${num(c.tsbks_20s.lsm)}, C4FM ${num(c.tsbks_20s.c4fm)}`]);
      if (c.carrier_offset_hz !== null && c.carrier_offset_hz !== undefined) rows.push(['Carrier offset', `${num(c.carrier_offset_hz)} Hz`]);
      rows.push([p.planLabel, num(c.channel_plan_entries)], ['Grants', num(c.grants)], ['CPU', pct(c.cpu_pct)]);
      control.body.replaceChildren(kv(rows));
    },
    unmount() {},
  };
}
