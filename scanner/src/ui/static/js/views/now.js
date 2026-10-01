// Now: the live site at a glance, or the way to add one.

import { h, card } from '../dom.js';
import { mhz, ago, pct, num, dur, dayTime, DASH, NOT_FOLLOWED, CLOSE_REASON } from '../format.js';
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
  const calls = card('Calls');
  el.append(h('div', { class: 'stack' }, site.el, calls.el, control.el));

  return {
    update(s) {
      const live = s.status && s.status.live;
      site.body.replaceChildren();
      control.el.hidden = !live || live.state !== 'live';
      calls.el.hidden = control.el.hidden;
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
      showCalls(calls, s);
    },
    unmount() {},
  };
}

// The open calls (lane first, then those not followed) and the newest closed ones.
function showCalls(c, s) {
  const v = s.calls || { open: [], recent: [] };
  const now = s.status.now_unix_ms;
  const who = x => (x.source ? ` from ${x.source}` : '');
  const where = x => `${x.channel || DASH} ${mhz(x.freq_hz)}`;
  const followed = v.open.filter(x => x.lane);
  const listed = v.open.filter(x => !x.lane);
  const rows = [];
  for (const x of followed) {
    rows.push(h('tr', null,
      h('td', { text: `Lane ${x.lane}` }),
      h('td', { text: `TG ${x.tg}${who(x)}` }),
      h('td', { text: where(x) }),
      h('td', { text: `${dur(now - x.started_unix_ms)}, ${num(x.voice_frames)} frames${x.end_lc ? ', ending' : ''}` })));
  }
  for (const x of listed) {
    rows.push(h('tr', { class: 'dim' },
      h('td', { text: DASH }),
      h('td', { text: `TG ${x.tg}${who(x)}` }),
      h('td', { text: where(x) }),
      h('td', { text: NOT_FOLLOWED[x.not_followed] || x.not_followed || 'not followed' })));
  }
  const open = rows.length ? h('table', { class: 'table' }, h('tbody', null, ...rows)) : h('p', { class: 'dim', text: 'No calls on the air.' });
  const recent = v.recent.slice(0, 15).map(x => h('tr', x.lane ? null : { class: 'dim' },
    h('td', { text: dayTime(x.started_unix_ms) }),
    h('td', { text: `TG ${x.tg}${who(x)}` }),
    h('td', { text: x.ended_unix_ms ? dur(x.ended_unix_ms - x.started_unix_ms) : DASH }),
    h('td', { text: x.lane ? `${num(x.voice_frames)} frames, ${CLOSE_REASON[x.close] || x.close || DASH}` : (NOT_FOLLOWED[x.not_followed] || x.not_followed || DASH) })));
  c.body.replaceChildren(open,
    h('h3', { text: 'Recent' }),
    recent.length ? h('table', { class: 'table' }, h('tbody', null, ...recent)) : h('p', { class: 'dim', text: 'None yet.' }));
}
