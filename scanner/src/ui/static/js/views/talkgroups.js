// The talkgroup bank on Now: every talkgroup of the live system that has an alias of its own or
// was heard in the last week, each with its controls as the front of a scanner has them: followed
// or not, its speaker, its priority and its name. A change goes to the radio at once (one
// talkgroup's controls); the radio pushes the system's aliases back and the rows follow them.
//
// Also what the rest of Now asks about the site's talkgroups: the ones known, and the ones a
// traffic channel can be kept on.

import { h, toast, setText, setClass, keyedList, icon } from '../dom.js';
import { api } from '../api.js';
import { boardNow } from '../store.js';
import { ago, num, DASH } from '../format.js';
import { aliasOf, routeOf, nameOf, colorOf } from '../aliases.js';

// The history is asked for the last week's talkgroups at most this often.
const HEARD_EVERY_MS = 300000;
// After a read that failed.
const HEARD_RETRY_MS = 15000;
// The lowest priority: what following a talkgroup gives it where only those with one are followed.
const LOWEST_PRIORITY = 100;
const SPEAKERS = ['both', 'left', 'right'];
const SPEAKER = { both: ['L+R', 'both speakers'], left: ['L', 'the left speaker'], right: ['R', 'the right speaker'] };

// The live site's talkgroups in the history's last week: each with its calls, how many were
// encrypted and when it was last heard. `version` moves when they are read.
const heard = { site: null, items: [], at: 0, version: 0 };

async function readHeard(site) {
  if (!site || (heard.site === site && Date.now() - heard.at < HEARD_EVERY_MS)) return;
  if (heard.site !== site) Object.assign(heard, { site, items: [], version: heard.version + 1 });
  heard.at = Date.now();
  try {
    const d = await api.activity('talkgroups', new URLSearchParams({ site, hours: '168', limit: '1000' }).toString());
    if (heard.site === site) Object.assign(heard, { items: d.items, version: heard.version + 1 });
  } catch {
    // Read again soon, not a whole period later.
    if (heard.site === site) heard.at = Date.now() - HEARD_EVERY_MS + HEARD_RETRY_MS;
  }
}

// The live site and its system in the store, or nulls.
function liveOf(s) {
  const lv = s && s.status && s.status.live;
  if (!lv || lv.state !== 'live') return { site: null, sys: null };
  return { site: lv.site.id, sys: (s.systems || []).find(x => x.id === lv.system.id) || null };
}

// Every talkgroup known at the live site, by name and then by number, those heard only encrypted
// last: { tg, name, alias, last (board ms, or null), encrypted (never heard in the clear) }.
export function knownTalkgroups(s) {
  const { site, sys } = liveOf(s);
  if (!site) return [];
  readHeard(site);
  const all = new Map();
  const at = tg => {
    if (!all.has(tg)) all.set(tg, { tg, calls: 0, enc: 0, last: null, heardName: null });
    return all.get(tg);
  };
  for (const a of (sys && sys.aliases) || []) {
    for (const id of a.ids || []) if (id.type === 'talkgroup') at(id.value);
  }
  for (const t of heard.site === site ? heard.items : []) {
    const e = at(t.tg);
    e.calls += t.calls;
    e.enc += t.encrypted;
    e.last = t.last_ms || e.last;
    e.heardName = t.alias || null;
  }
  for (const c of s.calls ? [...s.calls.open, ...s.calls.recent] : []) {
    if (c.private || c.site !== site) continue;
    const e = at(c.tg);
    e.calls += 1;
    e.enc += c.encrypted ? 1 : 0;
    e.last = Math.max(e.last || 0, c.started_unix_ms);
    e.heardName = e.heardName || c.tg_name || null;
  }
  return [...all.values()]
    .map(e => {
      const alias = aliasOf(sys, e.tg);
      return { tg: e.tg, alias, name: nameOf(e.tg, alias ? alias.name : e.heardName), last: e.last, encrypted: e.calls > 0 && e.enc === e.calls };
    })
    .sort((a, b) => a.encrypted - b.encrypted || (a.name || '￿').localeCompare(b.name || '￿') || a.tg - b.tg);
}

export const heardVersion = () => heard.version;

// The bank: `el` to show, `draw(store)` at each push, `stop()`.
export function talkgroupBank() {
  const count = h('span', { class: 'dim' });
  const search = h('input', { class: 'input', type: 'search', placeholder: 'Find', 'aria-label': 'Find a talkgroup' });
  const list = h('div', { class: 'tg-list' });
  const empty = h('div', { class: 'calls-empty' });
  const head = h('div', { class: 'tg-row tg-head', 'aria-hidden': 'true' }, h('span'), h('span'),
    h('span', { text: 'Priority', title: '1 (the highest) to 100; a higher priority takes a traffic channel from a lower one' }),
    h('span', { text: 'Speaker' }), h('span', { text: 'Follow' }));
  const el = h('section', { class: 'card flush now-bank', 'aria-label': 'Talkgroups' },
    h('div', { class: 'card-head' }, h('h2', { text: 'Talkgroups' }), count, h('div', { class: 'spacer' }), search),
    h('div', { class: 'card-body' }, head, list, empty));
  let last = null;
  // What the rows were last drawn from.
  let drawn = null;
  search.addEventListener('input', () => draw(last, true));

  // What was typed or set was refused: every row shows what the radio holds again, the field
  // being typed in too.
  function refused(message) {
    toast(message, true);
    if (list.contains(document.activeElement)) document.activeElement.blur();
    draw(last, true);
  }

  // One talkgroup's controls to the radio. The rows then show what the radio holds: the change
  // once it is pushed back, or what was there when it is refused.
  async function edit(tg, change) {
    const { sys } = liveOf(last);
    if (!sys) return;
    try {
      await api.setTalkgroup(sys.id, tg, change);
    } catch (e) {
      refused(e.message);
    }
  }

  function createRow() {
    const c = {
      dot: h('span', { class: 'tg-dot' }),
      name: h('input', { class: 'inline tg-name', type: 'text', spellcheck: 'false', autocomplete: 'off', maxlength: '60' }),
      sub: h('div', { class: 'sub' }),
      pri: h('input', { class: 'inline tg-pri', type: 'number', min: '1', max: String(LOWEST_PRIORITY), step: '1', placeholder: DASH }),
      spk: h('button', { class: 'btn small tg-spk', type: 'button' }),
      on: h('input', { type: 'checkbox', role: 'switch' }),
      enc: h('span', { class: 'tg-enc', title: 'Heard only encrypted: never followed' }, icon('lock')),
    };
    const row = h('div', { class: 'tg-row' },
      c.dot, h('div', { class: 'tg-text' }, c.name, c.sub), c.pri, c.spk, h('label', { class: 'switch' }, c.on), c.enc);
    row._c = c;
    c.name.addEventListener('change', () => edit(row._t.tg, { name: c.name.value.trim() || `TG ${row._t.tg}` }));
    c.name.addEventListener('keydown', e => {
      if (e.key === 'Enter') c.name.blur();
    });
    c.pri.addEventListener('change', () => {
      const text = c.pri.value.trim();
      const p = Number(text);
      if (text && !(Number.isInteger(p) && p >= 1 && p <= LOWEST_PRIORITY)) {
        refused(`A priority is 1 (the highest) to ${LOWEST_PRIORITY}, or empty for none`);
        return;
      }
      edit(row._t.tg, { priority: text ? p : null });
    });
    c.spk.addEventListener('click', () => {
      const next = SPEAKERS[(SPEAKERS.indexOf(row._route.speaker) + 1) % SPEAKERS.length];
      edit(row._t.tg, { speaker: next });
    });
    c.on.addEventListener('change', () => {
      const change = { do_not_monitor: !c.on.checked };
      // Where only talkgroups with a priority are followed, following one gives it the lowest.
      if (c.on.checked && row._route.needsPriority && row._route.priority === null) change.priority = LOWEST_PRIORITY;
      edit(row._t.tg, change);
    });
    return row;
  }

  // The controls from the radio's aliases; never under the field being typed in.
  function drawControls(row, t, sys) {
    const c = row._c;
    const r = row._route = routeOf(sys, t.tg);
    const label = `TG ${t.tg}`;
    c.dot.style.background = t.encrypted ? 'var(--idle)' : colorOf(t.tg, r.alias);
    c.name.placeholder = label;
    c.name.setAttribute('aria-label', `Name of ${label}`);
    if (document.activeElement !== c.name) c.name.value = t.name || '';
    c.pri.title = `Priority of ${t.name || label}: 1 (the highest) to ${LOWEST_PRIORITY}; empty: none`;
    c.pri.setAttribute('aria-label', c.pri.title);
    if (document.activeElement !== c.pri) c.pri.value = r.priority === null ? '' : String(r.priority);
    const [short, where] = SPEAKER[r.speaker] || SPEAKER.both;
    setText(c.spk, short);
    c.spk.title = `${t.name || label} plays on ${where}`;
    c.spk.setAttribute('aria-label', c.spk.title);
    c.on.checked = r.followed;
    c.on.title = r.followed ? `${t.name || label} is followed` : `${t.name || label} is not followed`;
    c.on.setAttribute('aria-label', `Follow ${t.name || label}`);
    for (const x of [c.pri, c.spk, c.on.parentElement]) x.hidden = t.encrypted;
    c.enc.hidden = !t.encrypted;
    setClass(row, 'off', !r.followed || t.encrypted);
  }

  function draw(s, force) {
    last = s;
    if (!s || !s.status) return;
    const { site, sys } = liveOf(s);
    const all = knownTalkgroups(s);
    const q = search.value.trim().toLowerCase();
    const items = q ? all.filter(t => String(t.tg).includes(q) || (t.name || '').toLowerCase().includes(q)) : all;
    const onAir = new Set(((s.calls && s.calls.open) || []).filter(c => c.site === site && !c.private).map(c => c.tg));
    const now = boardNow();
    const controls = [s.systems, heard.version, items.map(t => t.tg).join(',')];
    const fresh = force || !drawn || controls.some((v, i) => v !== drawn[i]);
    drawn = controls;
    keyedList(list, items, t => t.tg, createRow, (row, t) => {
      row._t = t;
      if (fresh) drawControls(row, t, sys);
      setClass(row, 'on-air', onAir.has(t.tg));
      setText(row._c.sub, [t.name ? `TG ${t.tg}` : '', t.encrypted ? 'encrypted' : '', onAir.has(t.tg) ? 'on the air' : t.last ? ago(now - t.last) : ''].filter(Boolean).join(' · '));
    });
    setText(count, all.length ? num(all.length) : '');
    count.title = 'Talkgroups with a name of their own, and those heard in the last week';
    empty.hidden = items.length > 0;
    head.hidden = !items.length;
    setText(empty, !site ? 'No site is live.' : all.length ? 'No talkgroup matches.' : 'No talkgroups heard yet.');
  }

  return { el, draw, stop() {} };
}
