// Now: the live site at a glance, or the way to add one; hold the site on a talkgroup.

import { h, card, toast } from '../dom.js';
import { api } from '../api.js';
import { refresh } from '../store.js';
import { mhz, ago, pct, num, dur, bytes, dayTime, DASH, NOT_FOLLOWED, CLOSE_REASON } from '../format.js';
import { protocol } from '../protocols.js';

// A control channel with no message for this long is shown as silent.
const SILENT_MS = 5000;

// Hold the live site on `tg`, or release the hold (null).
async function setHold(tg) {
  try {
    await api.setHold(tg);
    toast(tg === null ? 'Hold released' : `Holding TG ${tg}`);
    await refresh();
  } catch (e) {
    toast(e.message, true);
  }
}

function holdButton(tg, held) {
  return held === tg
    ? h('button', { class: 'btn small', type: 'button', text: 'Release', onclick: () => setHold(null) })
    : h('button', { class: 'btn small', type: 'button', text: 'Hold', title: `Follow only TG ${tg}`, onclick: () => setHold(tg) });
}

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
  // One player for the recordings, outside the body the polls rebuild, so playback survives them.
  const clip = h('audio', { controls: true, preload: 'none', hidden: true, style: { width: '100%', marginTop: '10px' } });
  calls.el.append(clip);
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
      if (live.state === 'scanning') {
        site.body.append(h('p', null, 'Scanning for systems (', h('a', { href: '#systems', text: 'Systems' }), '); the live site is paused.'));
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
      ]), s.status.hold
        ? h('div', { class: 'row' }, h('strong', { text: `Holding TG ${s.status.hold}` }),
          h('span', { class: 'dim', text: ': no other talkgroup is followed' }), h('div', { class: 'spacer' }), holdButton(s.status.hold, s.status.hold))
        : h('p', { class: 'card-note' }, 'Following ', live.profile ? h('strong', { text: live.profile.name }) : 'every clear call',
          ' (', h('a', { href: '#settings', text: 'profiles in Settings' }), ').'));

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
      rows.push(...p.healthRows(c));
      rows.push([p.planLabel, num(c.channel_plan_entries)], ['Grants', num(c.grants)], ['CPU', pct(c.cpu_pct)]);
      control.body.replaceChildren(kv(rows));
      showCalls(calls, s, clip);
    },
    unmount() {},
  };
}

function play(clip, r) {
  clip.hidden = false;
  clip.src = `/api/v1/recordings/${r.id}.wav`;
  clip.play().catch(() => {});
}

function recordingNote(st) {
  if (!st) return '';
  if (!st.enabled) return 'Recording is off (Settings).';
  const where = st.store === 'sd' ? 'SD card' : 'RAM';
  const sd = st.sd_status.state === 'ok' || st.store !== 'sd' ? '' : ` (SD card ${st.sd_status.state.replace('_', ' ')}: saving to RAM)`;
  return `Recording to ${where}${sd}: ${num(st.sd.count + st.ram.count)} kept, ${bytes(st.sd.bytes + st.ram.bytes)}.`;
}

// The open calls (lane first, then those not followed) and the newest closed ones, with their
// recordings.
function showCalls(c, s, clip) {
  const v = s.calls || { open: [], recent: [] };
  const now = s.status.now_unix_ms;
  const held = s.status.hold || null;
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
      h('td', { text: `${dur(now - x.started_unix_ms)}, ${num(x.voice_frames)} frames${x.end_lc ? ', ending' : ''}` }),
      h('td', null, holdButton(x.tg, held))));
  }
  for (const x of listed) {
    rows.push(h('tr', { class: 'dim' },
      h('td', { text: DASH }),
      h('td', { text: `TG ${x.tg}${who(x)}` }),
      h('td', { text: where(x) }),
      h('td', { text: NOT_FOLLOWED[x.not_followed] || x.not_followed || 'not followed' }),
      h('td', null, holdButton(x.tg, held))));
  }
  const open = rows.length ? h('table', { class: 'table' }, h('tbody', null, ...rows)) : h('p', { class: 'dim', text: 'No calls on the air.' });
  const recs = new Map(((s.recordings && s.recordings.items) || []).map(r => [r.id, r]));
  const recent = v.recent.slice(0, 15).map(x => {
    const r = recs.get(x.call);
    return h('tr', x.lane ? null : { class: 'dim' },
      h('td', { text: dayTime(x.started_unix_ms) }),
      h('td', { text: `TG ${x.tg}${who(x)}` }),
      h('td', { text: x.ended_unix_ms ? dur(x.ended_unix_ms - x.started_unix_ms) : DASH }),
      h('td', { text: x.lane ? `${num(x.voice_frames)} frames, ${CLOSE_REASON[x.close] || x.close || DASH}` : (NOT_FOLLOWED[x.not_followed] || x.not_followed || DASH) }),
      h('td', null, r ? h('button', {
        class: 'btn small', type: 'button', text: 'Play',
        title: `${dur(r.duration_ms)} recorded, ${r.store === 'sd' ? 'SD card' : 'RAM'}`,
        onclick: () => play(clip, r),
      }) : null),
      h('td', null, holdButton(x.tg, held)));
  });
  c.body.replaceChildren(open,
    h('h3', { text: 'Recent' }),
    recent.length ? h('table', { class: 'table' }, h('tbody', null, ...recent)) : h('p', { class: 'dim', text: 'None yet.' }),
    h('p', { class: 'card-note', text: recordingNote(s.recordings && s.recordings.storage) }));
}
