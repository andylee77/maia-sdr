// Event-log viewer (/api/log): newest entries first loaded with
// ?tail=1, then followed incrementally with ?since=<seq>. Control-
// channel TSBK mirror lines ("grant" entries with fields.event_type,
// most of the ring on a busy site) are shown as their own "CC" group,
// off by default, and filtered server-side (?tsbk=0) while hidden.

import { h, setText, card } from '../dom.js';
import { clockMs } from '../format.js';
import { api } from '../api.js';

const GROUPS = [
  { id: 'calls', label: 'Calls', cats: ['traffic', 'recorder', 'grant'], on: true },
  { id: 'system', label: 'System', cats: ['system'], on: true },
  { id: 'vocoder', label: 'Vocoder', cats: ['vocoder'], on: false },
  { id: 'cc', label: 'CC messages', cats: ['cc'], on: false },
  { id: 'voice', label: 'Voice / DUID', cats: ['voice', 'duid'], on: false },
];
const MAX_LINES = 1000;
const POLL_MS = 2000;

function catOf(e) {
  return e.category === 'grant' && e.fields && e.fields.event_type ? 'cc' : e.category;
}

// Recorder entries carry their meaning in fields ("call_saved" alone
// says little); give them a readable one-line summary.
function messageText(e) {
  const f = e.fields || {};
  if (e.category !== 'recorder') return e.message;
  const bits = [e.message.replace(/_/g, ' ')];
  const id = f.recording_id ?? f.event_call_id;
  if (id !== undefined) bits.push('#' + id);
  if (f.tg) bits.push('TG ' + f.tg);
  if (f.source) bits.push('from ' + f.source);
  if (f.duration_ms !== undefined) bits.push((f.duration_ms / 1000).toFixed(1) + ' s');
  if (f.reason) bits.push('(' + String(f.reason).replace(/_/g, ' ') + ')');
  return bits.join(' ');
}

function fieldsText(f) {
  if (!f || typeof f !== 'object') return '';
  const parts = [];
  for (const [k, v] of Object.entries(f)) {
    if (v === null || v === undefined || k === 'summary' || k === 'timestamp') continue;
    parts.push(k + '=' + (typeof v === 'object' ? JSON.stringify(v) : v));
  }
  return parts.length ? ' {' + parts.join(' ') + '}' : '';
}

export function logView() {
  const c = card('Event log');
  const on = Object.fromEntries(GROUPS.map(g => [g.id, g.on]));
  const chips = GROUPS.map(g => {
    const b = h('button', { class: 'chip' + (g.on ? ' on' : ''), type: 'button', text: g.label });
    b.addEventListener('click', () => {
      on[g.id] = !on[g.id];
      b.classList.toggle('on', on[g.id]);
      if (g.id === 'cc') reload(); else renderAll();
    });
    return b;
  });
  const filter = h('input', { class: 'input', type: 'search', placeholder: 'Filter text', 'aria-label': 'Filter log' });
  filter.addEventListener('input', () => renderAll());
  const pause = h('button', { class: 'chip', type: 'button', text: 'Pause' });
  const fields = h('button', { class: 'chip', type: 'button', text: 'Fields' });
  const status = h('span', { class: 'dim', style: { fontSize: '0.84em' } });
  c.right.append(status);
  const pane = h('div', { class: 'log', role: 'log' });
  c.body.append(h('div', { class: 'row', style: { marginBottom: '10px' } }, chips, filter, pause, fields), pane);

  let entries = [];
  let lastSeq = 0;
  let paused = false;
  let showFields = false;
  let timer = null;
  let gen = 0;

  pause.addEventListener('click', () => { paused = !paused; pause.classList.toggle('on', paused); setText(pause, paused ? 'Paused' : 'Pause'); });
  fields.addEventListener('click', () => { showFields = !showFields; fields.classList.toggle('on', showFields); renderAll(); });

  const visibleCats = () => new Set(GROUPS.filter(g => on[g.id]).flatMap(g => g.cats));

  function line(e) {
    const cat = catOf(e);
    return h('div', { class: 'log-line' },
      h('span', { class: 't', text: clockMs(e.timestamp_ms) }),
      h('span', { class: 'c c-' + cat, text: cat }),
      h('span', { class: 'm' }, messageText(e), showFields ? h('span', { class: 'f', text: fieldsText(e.fields) }) : null));
  }

  function matches(e, cats, q) {
    if (!cats.has(catOf(e))) return false;
    if (!q) return true;
    return (messageText(e) + fieldsText(e.fields)).toLowerCase().includes(q);
  }

  function renderAll() {
    const cats = visibleCats();
    const q = filter.value.trim().toLowerCase();
    pane.replaceChildren(...entries.filter(e => matches(e, cats, q)).map(line));
    pane.scrollTop = pane.scrollHeight;
    setText(status, entries.length + ' loaded · seq ' + lastSeq);
  }

  function appendNew(list) {
    const atBottom = pane.scrollTop + pane.clientHeight >= pane.scrollHeight - 30;
    entries.push(...list);
    if (entries.length > MAX_LINES) {
      entries = entries.slice(-MAX_LINES);
      renderAll();
      return;
    }
    const cats = visibleCats();
    const q = filter.value.trim().toLowerCase();
    for (const e of list) if (matches(e, cats, q)) pane.appendChild(line(e));
    while (pane.childElementCount > MAX_LINES) pane.firstChild.remove();
    if (atBottom) pane.scrollTop = pane.scrollHeight;
    setText(status, entries.length + ' loaded · seq ' + lastSeq);
  }

  async function reload() {
    const my = ++gen;
    try {
      const r = await api.log({ tail: 1, limit: 400, tsbk: on.cc ? 1 : 0 });
      if (my !== gen) return;
      entries = r.entries || [];
      lastSeq = r.last_seq || 0;
      renderAll();
    } catch (e) {
      setText(status, 'log unavailable: ' + e.message);
    }
  }

  async function poll() {
    if (paused) return;
    const my = gen;
    try {
      let more = true;
      while (more && my === gen) {
        const r = await api.log({ since: lastSeq, limit: 500, tsbk: on.cc ? 1 : 0 });
        if (my !== gen) return;
        const list = r.entries || [];
        if (list.length) lastSeq = list[list.length - 1].seq;
        else lastSeq = Math.max(lastSeq, r.last_seq || 0);
        if (list.length) appendNew(list);
        more = list.length === 500;
      }
    } catch { /* next tick */ }
  }

  reload();
  timer = setInterval(poll, POLL_MS);
  return { el: c.el, unmount: () => clearInterval(timer) };
}
