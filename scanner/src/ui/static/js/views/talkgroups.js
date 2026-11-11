// Now's talkgroups: the live site's, as the radio pushes them (how each is treated, and what
// decides recording), in three groups: those listened to, those switched off, and those heard
// only encrypted. A row has a talkgroup's tuner, its Record mark and its Listen switch. One
// talkgroup is selected, and the bar under the list holds its controls: the list is what a
// handheld's knob steps through, the bar its buttons. Every control is one write to the radio,
// which pushes the list back.

import { h, toast, setText, setClass, keyedList, icon } from '../dom.js';
import { api } from '../api.js';
import { boardNow } from '../store.js';
import { ago, num } from '../format.js';
import { colorOf } from '../aliases.js';

const LOWEST_PRIORITY = 100;
// A talkgroup's traffic lane as the radio spells it, in the order its chip steps through:
// [speaker, on the chip, in the bar, said in full].
const TUNERS = [
  ['both', 'Any', 'Any tuner', 'either tuner'],
  ['left', '1', 'Traffic 1', 'Traffic 1 only'],
  ['right', '2', 'Traffic 2', 'Traffic 2 only'],
];
const tunerOf = speaker => TUNERS.find(x => x[0] === speaker) || TUNERS[0];

// The groups of the list, in its order. The first is always open.
const GROUPS = [
  { kind: 'on', title: 'Listening' },
  { kind: 'off', title: 'Switched off' },
  { kind: 'enc', title: 'Heard only encrypted' },
];

let memo = { key: [], list: [] };

// The talkgroups to list, by name and then number, from what the radio pushed and the calls on
// the page. Each is the pushed talkgroup with `label`, `color`, `air` (the lane it is on the air
// on, 0 on the air and not followed, null not on the air), `last` (when last heard, board ms) and
// `kind` (its group).
export function talkgroupsOf(s) {
  const v = s.talkgroups;
  if (!v || !v.site) return [];
  const open = (s.calls && s.calls.open) || [];
  const recent = (s.calls && s.calls.recent) || [];
  const key = [v, recent, open.map(c => `${c.call}:${c.tg}:${c.lane}`).join()];
  if (key.every((x, i) => x === memo.key[i])) return memo.list;
  const air = new Map();
  const last = new Map();
  for (const c of [...open, ...recent]) {
    if (c.private || c.site !== v.site) continue;
    last.set(c.tg, Math.max(last.get(c.tg) || 0, c.started_unix_ms));
  }
  for (const c of open) if (!c.private && c.site === v.site) air.set(c.tg, c.lane || 0);
  const list = v.items.map(t => ({
    ...t,
    label: t.name || `TG ${t.tg}`,
    color: colorOf(t.tg, t),
    air: air.has(t.tg) ? air.get(t.tg) : null,
    last: Math.max(t.last_ms || 0, last.get(t.tg) || 0) || null,
    kind: t.calls > 0 && t.encrypted === t.calls ? 'enc' : t.followed ? 'on' : 'off',
  })).sort((a, b) => (a.name || '￿').localeCompare(b.name || '￿') || a.tg - b.tg);
  memo = { key, list };
  return list;
}

// The list and the selected talkgroup's bar: `el` to show, `draw(store)` at each push, `key(e)`
// for the page's keys. `hold(tg)` holds the radio on a talkgroup, or releases it (null).
export function talkgroupList({ hold }) {
  const count = h('span', { class: 'dim' });
  const search = h('input', { class: 'input', type: 'search', placeholder: 'Find', 'aria-label': 'Find a talkgroup' });
  const empty = h('div', { class: 'calls-empty' });
  // Each group: its rows, and its heading (the first group's is the columns' headings; the
  // others' opens and closes the group, and says what a closed one hides).
  const groups = GROUPS.map(g => {
    const rows = h('div', { class: 'tgl-rows' });
    const title = h('span');
    if (g.kind === 'on') {
      const head = h('div', { class: 'tgl-row tgl-cols', 'aria-hidden': 'true' }, title,
        h('span', { text: 'Tuner', title: 'The traffic tuner that follows it: either, or always the same one' }),
        h('span', { text: 'Rec' }), h('span', { text: 'Listen' }));
      return { kind: g.kind, name: g.title, rows, head, title, newest: null };
    }
    const newest = h('span', { class: 'tgl-hint' });
    const head = h('button', { class: 'tgl-group', type: 'button', 'aria-expanded': 'false' }, icon('down', 14), title, newest);
    head.addEventListener('click', () => {
      const open = head.getAttribute('aria-expanded') === 'true';
      opened[g.kind] = !open;
      // A group closed over the selected talkgroup lets it go.
      if (open && chosen && chosen.kind === g.kind) selected = null;
      draw(last, true);
    });
    return { kind: g.kind, name: g.title, rows, head, title, newest };
  });
  const body = h('div', { class: 'card-body' }, ...groups.slice(1).flatMap(g => [g.head, g.rows]), empty);
  body.prepend(groups[0].rows);

  // The selected talkgroup's bar.
  const b = {
    dot: h('span', { class: 'tg-dot' }),
    name: h('strong', { class: 'tgl-name' }),
    rename: h('input', { class: 'input tgl-rename', type: 'text', maxlength: '60', spellcheck: 'false', autocomplete: 'off', hidden: true, 'aria-label': 'Its name' }),
    sub: h('span', { class: 'dim' }),
    pri: h('input', { class: 'input tgl-pri', type: 'number', min: '1', max: String(LOWEST_PRIORITY), step: '1', placeholder: 'none', 'aria-label': 'Priority, 1 the highest' }),
    hold: h('button', { class: 'btn small', type: 'button' }),
    follow: h('button', { class: 'btn small', type: 'button' }),
    rec: h('button', { class: 'btn small', type: 'button' }, h('span', { class: 'tgl-recdot' }), 'Record'),
    tuner: TUNERS.map(([speaker, , text, full]) => h('button', { type: 'button', text, title: `Followed by ${full}`, dataset: { speaker } })),
    edit: h('button', { class: 'btn small quiet', type: 'button', text: 'Rename' }),
  };
  const hint = h('div', { class: 'tgl-pick dim', text: 'Pick a talkgroup for its controls. The arrow keys step through the list.' });
  const controls = h('div', { class: 'tgl-controls', hidden: true },
    h('div', { class: 'row' }, b.dot, b.name, b.rename, b.sub, h('div', { class: 'spacer' }),
      h('label', { class: 'row tgl-pri-label', title: '1 (the highest) to 100; a higher priority takes a tuner from a lower one' }, h('span', { class: 'dim', text: 'Priority' }), b.pri)),
    h('div', { class: 'row' }, b.hold, b.follow, b.rec, h('div', { class: 'seg', role: 'group', 'aria-label': 'Its tuner' }, ...b.tuner), h('div', { class: 'spacer' }), b.edit));
  const bar = h('div', { class: 'tgl-bar' }, hint, controls);

  const el = h('section', { class: 'card flush now-list', 'aria-label': 'Talkgroups' },
    h('div', { class: 'card-head' }, h('h2', { text: 'Talkgroups' }), count, h('div', { class: 'spacer' }), search),
    groups[0].head, body, bar);

  let last = null;
  // The pushed talkgroups the rows' controls were last drawn from.
  let drawnFrom = null;
  let selected = null;
  // The selected talkgroup, as last drawn.
  let chosen = null;
  // The groups opened by hand.
  const opened = { off: false, enc: false };
  // The bar's name is being typed.
  let renaming = false;
  search.addEventListener('input', () => draw(last, true));

  const system = () => last && last.talkgroups && last.talkgroups.system;
  const policy = () => (last && last.talkgroups && last.talkgroups.recording) || { enabled: false, every_call: false };

  // One talkgroup's controls to the radio, which pushes the list back. A refusal is shown, and
  // the rows show what the radio holds again.
  async function edit(t, change) {
    if (!system()) return;
    try {
      await api.setTalkgroup(system(), t.tg, change);
    } catch (e) {
      toast(e.message, true);
      draw(last, true);
    }
  }

  function select(tg) {
    if (tg === selected) return;
    selected = tg;
    endRename(false);
    // The rows' marks move; their controls stay as they are until the radio pushes.
    draw(last);
    const row = body.querySelector('.tgl-row.sel');
    if (row) row.scrollIntoView({ block: 'nearest' });
  }

  function createRow() {
    const c = {
      dot: h('span', { class: 'tg-dot' }),
      name: h('div', { class: 'tgl-name' }),
      sub: h('div', { class: 'sub' }),
      tuner: h('button', { class: 'tgl-chip', type: 'button' }),
      rec: h('button', { class: 'tgl-rec', type: 'button' }),
      on: h('input', { type: 'checkbox', role: 'switch' }),
      lock: h('span', { class: 'tgl-lock', title: 'Heard only encrypted: never followed' }, icon('lock')),
    };
    const sw = h('label', { class: 'switch' }, c.on);
    const row = h('div', { class: 'tgl-row' }, h('div', { class: 'tgl-what' }, c.dot, h('div', { class: 'tgl-text' }, c.name, c.sub)), c.tuner, c.rec, sw, c.lock);
    row._c = c;
    row.addEventListener('click', e => {
      if (!e.target.closest('button, label')) select(row._t.tg);
    });
    c.tuner.addEventListener('click', () => {
      const i = TUNERS.findIndex(x => x[0] === row._t.speaker);
      select(row._t.tg);
      edit(row._t, { speaker: TUNERS[(i + 1) % TUNERS.length][0] });
    });
    c.rec.addEventListener('click', () => {
      select(row._t.tg);
      edit(row._t, { record: !row._t.record });
    });
    c.on.addEventListener('change', () => {
      const follow = c.on.checked;
      select(row._t.tg);
      edit(row._t, { follow });
    });
    return row;
  }

  // A Record control's state under the policy: [lit, usable, what it says].
  function recState(t, p) {
    if (!p.enabled) return [false, false, 'Recording is off'];
    if (p.every_call) return [t.followed, false, 'Every followed call is recorded'];
    return [t.record, true, t.record ? `${t.label}: its calls are recorded` : `Record the calls of ${t.label}`];
  }

  function updateRow(row, t, fresh, now) {
    const c = row._c;
    const isNew = row._t === undefined;
    row._t = t;
    setClass(row, 'sel', t.tg === selected);
    setClass(row, 'air', t.air !== null);
    setClass(row, 'off', t.kind !== 'on');
    setText(c.sub, [
      t.name ? `TG ${t.tg}` : '',
      t.air ? `on the air · Traffic ${t.air}` : t.air === 0 ? 'on the air' : t.last ? ago(now - t.last) : 'not heard this week',
    ].filter(Boolean).join(' · '));
    // The controls from what the radio pushed; not at every tick, so a switch just thrown stays
    // until the radio answers.
    if (!fresh && !isNew) return;
    c.dot.style.background = t.kind === 'enc' ? 'var(--idle)' : t.color;
    setText(c.name, t.label);
    const enc = t.kind === 'enc';
    for (const x of [c.tuner, c.rec, c.on.parentElement]) x.hidden = enc;
    c.lock.hidden = !enc;
    const [, chip, , full] = tunerOf(t.speaker);
    setText(c.tuner, chip);
    c.tuner.title = `${t.label} is followed by ${full}`;
    c.tuner.setAttribute('aria-label', c.tuner.title);
    setClass(c.tuner, 'set', t.speaker !== 'both');
    const [lit, usable, says] = recState(t, policy());
    setClass(c.rec, 'on', lit);
    c.rec.disabled = !usable;
    c.rec.title = says;
    c.rec.setAttribute('aria-label', says);
    c.rec.setAttribute('aria-pressed', String(lit));
    c.on.checked = t.followed;
    c.on.setAttribute('aria-label', `Listen to ${t.label}`);
    c.on.parentElement.title = t.followed ? `${t.label} is listened to` : `${t.label} is switched off`;
  }

  // The name being typed is kept (`save`) or dropped.
  function endRename(save) {
    if (!renaming) return;
    renaming = false;
    const t = chosen;
    const name = b.rename.value.trim();
    b.rename.hidden = true;
    b.name.hidden = false;
    if (!save || !t || name === (t.name || '')) return;
    if (name) {
      edit(t, { name });
      return;
    }
    // An emptied name: a talkgroup with nothing else of its own loses its alias; one with
    // settings keeps them under its number.
    const l = last.talkgroups.listening || {};
    const plain = t.own && t.priority === null && !t.record && t.speaker === l.unmonitored_speaker && t.followed === l.follow_unmonitored;
    if (plain) api.forgetTalkgroup(system(), t.tg).catch(e => toast(e.message, true));
    else edit(t, { name: `TG ${t.tg}` });
  }
  b.edit.addEventListener('click', () => {
    if (!chosen) return;
    renaming = true;
    b.rename.value = chosen.name || '';
    b.rename.placeholder = `TG ${chosen.tg}`;
    b.name.hidden = true;
    b.rename.hidden = false;
    b.rename.focus();
    b.rename.select();
  });
  b.rename.addEventListener('keydown', e => {
    if (e.key === 'Enter') endRename(true);
    else if (e.key === 'Escape') endRename(false);
  });
  b.rename.addEventListener('blur', () => endRename(true));
  b.pri.addEventListener('change', () => {
    if (!chosen) return;
    const text = b.pri.value.trim();
    const p = Number(text);
    if (text && !(Number.isInteger(p) && p >= 1 && p <= LOWEST_PRIORITY)) {
      toast(`A priority is 1 (the highest) to ${LOWEST_PRIORITY}, or empty for none`, true);
      b.pri.value = chosen.priority === null ? '' : String(chosen.priority);
      return;
    }
    edit(chosen, { priority: text ? p : null });
  });
  const held = () => (last && last.status && last.status.hold) || null;
  const toggleHold = () => chosen && chosen.kind !== 'enc' && hold(held() === chosen.tg ? null : chosen.tg);
  const toggleFollow = () => chosen && chosen.kind !== 'enc' && edit(chosen, { follow: !chosen.followed });
  const toggleRecord = () => chosen && recState(chosen, policy())[1] && edit(chosen, { record: !chosen.record });
  const setTuner = speaker => chosen && chosen.kind !== 'enc' && chosen.speaker !== speaker && edit(chosen, { speaker });
  b.hold.addEventListener('click', toggleHold);
  b.follow.addEventListener('click', toggleFollow);
  b.rec.addEventListener('click', toggleRecord);
  for (const x of b.tuner) x.addEventListener('click', () => setTuner(x.dataset.speaker));

  function drawBar(t) {
    chosen = t;
    hint.hidden = !!t;
    controls.hidden = !t;
    if (!t) return;
    const enc = t.kind === 'enc';
    b.dot.style.background = enc ? 'var(--idle)' : t.color;
    setText(b.name, t.label);
    setText(b.sub, [t.name ? `TG ${t.tg}` : '', t.calls ? `${num(t.calls)} call${t.calls === 1 ? '' : 's'} this week` : '', enc ? 'only encrypted: never followed' : ''].filter(Boolean).join(' · '));
    if (document.activeElement !== b.pri) b.pri.value = t.priority === null ? '' : String(t.priority);
    b.pri.disabled = enc;
    const holding = held() === t.tg;
    setText(b.hold, holding ? 'Release' : 'Hold');
    b.hold.title = holding ? 'Follow every talkgroup again' : `Follow only ${t.label}, on both tuners, until released`;
    b.hold.setAttribute('aria-pressed', String(holding));
    setClass(b.hold, 'lit', holding);
    setText(b.follow, t.followed ? 'Skip' : 'Listen');
    b.follow.title = t.followed ? `Switch ${t.label} off: its calls are not followed` : `Listen to ${t.label} again`;
    const [lit, usable, says] = recState(t, policy());
    setClass(b.rec, 'lit', lit);
    b.rec.disabled = !usable || enc;
    b.rec.title = says;
    b.rec.setAttribute('aria-pressed', String(lit));
    for (const x of b.tuner) {
      const on = x.dataset.speaker === t.speaker;
      setClass(x, 'on', on);
      x.setAttribute('aria-pressed', String(on));
      x.disabled = enc;
    }
    b.hold.disabled = b.follow.disabled = enc;
  }

  function draw(s, force) {
    last = s;
    if (!s || !s.status) return;
    const v = s.talkgroups;
    const all = talkgroupsOf(s);
    const q = search.value.trim().toLowerCase();
    const match = t => !q || String(t.tg).includes(q) || t.label.toLowerCase().includes(q);
    const fresh = force || v !== drawnFrom;
    drawnFrom = v;
    const now = boardNow();
    if (selected !== null && !all.some(t => t.tg === selected)) selected = null;
    let shownRows = 0;
    for (const g of groups) {
      const items = all.filter(t => t.kind === g.kind);
      const shown = items.filter(match);
      // A group is open by hand, while something is searched for, or around the selected
      // talkgroup: one just switched off stays in view.
      const open = g.kind === 'on' || opened[g.kind] || !!q || items.some(t => t.tg === selected);
      keyedList(g.rows, open ? shown : [], t => t.tg, createRow, (row, t) => updateRow(row, t, fresh, now));
      g.rows.hidden = !open || !shown.length;
      shownRows += open ? shown.length : 0;
      setText(g.title, `${g.name} · ${num(items.length)}`);
      if (g.kind === 'on') continue;
      g.head.hidden = !items.length;
      g.head.setAttribute('aria-expanded', String(open));
      setClass(g.head, 'open', open);
      // What a closed group hides: its talkgroup heard last.
      const newest = items.reduce((a, t) => (t.last && (!a || t.last > a.last) ? t : a), null);
      setText(g.newest, open || !newest ? '' : `${newest.label} heard ${ago(now - newest.last)}`);
    }
    groups[0].head.hidden = !all.length;
    setText(count, all.length ? num(all.length) : '');
    count.title = 'The talkgroups an alias names, and those heard here in the last week';
    empty.hidden = shownRows > 0 || (!q && all.length > 0);
    setText(empty, !v || !v.site ? 'No site is live.' : all.length ? 'No talkgroup matches.' : 'No talkgroups heard yet.');
    drawBar(all.find(t => t.tg === selected) || null);
  }

  // The rows on screen, in order: what the arrow keys step through.
  const visible = () => [...body.querySelectorAll('.tgl-row')].filter(r => !r.parentElement.hidden).map(r => r._t);

  // The page's keys, away from any field: the arrows step through the list, and the selected
  // talkgroup takes H (hold), L (listen or skip), R (record), 0 1 2 (its tuner).
  function key(e) {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      const list = visible();
      if (!list.length) return false;
      const i = list.findIndex(t => t.tg === selected);
      const next = i < 0 ? 0 : Math.max(0, Math.min(list.length - 1, i + (e.key === 'ArrowDown' ? 1 : -1)));
      select(list[next].tg);
      return true;
    }
    if (!chosen) return false;
    const k = e.key.toLowerCase();
    if (k === 'h') toggleHold();
    else if (k === 'l') toggleFollow();
    else if (k === 'r') toggleRecord();
    else if (k === '0' || k === '1' || k === '2') setTuner(TUNERS[Number(k)][0]);
    else return false;
    return true;
  }

  return { el, draw, key, select };
}
