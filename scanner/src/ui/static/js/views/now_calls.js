// Now's calls: every call of the live site, newest first (those on the air, then every closed
// one; the radio pushes the newest, older ones are read from the history as the list reaches its
// end), and the picked call beside the list (call_rows.js draws the rows and the panel).

import { h, setText, setClass, keyedList } from '../dom.js';
import { api } from '../api.js';
import { boardNow } from '../store.js';
import { num, bytes } from '../format.js';
import { callKey, recordingOf, alertsOf, detailPanel, createRow, updateRow } from './call_rows.js';

// The call list's ages move between pushes.
const AGES_MS = 5000;
// Calls read from the history a page at a time as the list is scrolled to its end.
const PAGE = 100;
const FILTERS = [['all', 'All', 'Every call'], ['followed', 'Followed', 'Only the calls a traffic channel followed'], ['alerts', 'Alerts', 'Only the calls with an alert tone']];

// Kept while the page is left for another.
let filter = 'all';

const known = v => v !== null && v !== undefined;

function recordingNote(st) {
  if (!st) return '';
  if (!st.enabled) return 'Recording is off (Settings).';
  const where = st.store === 'sd' ? 'SD card' : 'RAM';
  const state = st.sd_status.state;
  const sd = state === 'ok' || st.store !== 'sd' ? '' : state === 'failing' ? ' (SD card writes failing: saving to RAM until one lands)' : ` (SD card ${state.replace('_', ' ')}: saving to RAM)`;
  return `Recording to ${where}${sd}: ${num(st.sd.count + st.ram.count)} kept, ${bytes(st.sd.bytes + st.ram.bytes)}.`;
}

// The calls and the picked call's panel: `list` and `detail` to show, `draw(store)` at each
// push, `stop()`. Closed calls stay in the list once seen, so a long session keeps them all.
export function callsPane() {
  const count = h('span', { class: 'dim' });
  const search = h('input', { class: 'input', type: 'search', placeholder: 'Talkgroup or radio', 'aria-label': 'Filter the calls' });
  const chips = FILTERS.map(([value, text, title]) => h('button', { class: 'chip', type: 'button', text, title, onclick: () => {
    filter = value;
    draw(s, true);
  } }));
  const rows = h('div', { class: 'calls' });
  const empty = h('div', { class: 'calls-empty' });
  const moreBtn = h('button', { class: 'btn small', type: 'button', text: 'Load older calls' });
  const moreText = h('span');
  const more = h('div', { class: 'calls-more' }, moreText, moreBtn);
  const list = h('section', { class: 'card flush now-calls', 'aria-label': 'Calls' },
    h('div', { class: 'card-head' }, h('h2', { text: 'Calls' }), count, h('div', { class: 'spacer' }), h('div', { class: 'row', role: 'group', 'aria-label': 'Which calls' }, ...chips), search),
    h('div', { class: 'card-body' }, rows, empty, more));
  let s = null;
  let drawn = null;
  // The picked call's key, and a play asked for with the pick.
  let picked = null;
  let playAsked = false;
  const detail = detailPanel(() => {
    picked = null;
    draw(s, true);
  });
  const pick = (x, play) => {
    picked = callKey(x);
    playAsked = play;
    draw(s, true);
  };
  // This site's closed calls by key, and the history's paging.
  const book = { site: null, closed: new Map(), done: false, loading: false, error: null, total: null, totalAt: 0, version: 0 };
  search.addEventListener('input', () => draw(s, true));
  moreBtn.addEventListener('click', () => loadOlder());

  const liveSite = () => {
    const lv = s && s.status && s.status.live;
    return lv && lv.state === 'live' ? lv.site.id : null;
  };
  const filtering = () => !!search.value.trim() || filter === 'alerts';

  // The end of the list in view: the next page (only while no filter could hide what it reads).
  const io = new IntersectionObserver(entries => {
    if (entries.some(e => e.isIntersecting) && !filtering()) loadOlder();
  });
  io.observe(more);

  async function loadOlder() {
    const site = liveSite();
    if (!site || book.loading || book.done) return;
    let oldest = Infinity;
    for (const x of book.closed.values()) oldest = Math.min(oldest, x.started_unix_ms);
    if (oldest === Infinity) oldest = boardNow() + 1;
    book.loading = true;
    book.error = null;
    draw(s, true);
    try {
      const q = new URLSearchParams({ site, from: '1', to: String(oldest), limit: String(PAGE) });
      const r = await api.activity('calls', q.toString());
      if (book.site !== site) return;
      for (const x of r.items) if (!book.closed.has(callKey(x))) book.closed.set(callKey(x), x);
      book.done = r.items.length < PAGE;
      book.version++;
    } catch (e) {
      book.error = e.message;
    } finally {
      book.loading = false;
      draw(s, true);
      // Read the next page at once while the end is still in view.
      io.unobserve(more);
      io.observe(more);
    }
  }

  // The site's calls in the history, at most every 30 s.
  async function readTotal(site) {
    if (Date.now() - book.totalAt < 30000) return;
    book.totalAt = Date.now();
    try {
      const d = await api.activity('sites');
      const st = d.sites.find(x => x.site === site);
      if (book.site === site) book.total = st ? st.calls : 0;
      draw(s, true);
    } catch { /* read again later */ }
  }

  function draw(store, force) {
    s = store;
    if (!s || !s.status) return;
    const site = liveSite();
    if (site !== book.site) {
      Object.assign(book, { site, closed: new Map(), done: false, error: null, total: null, totalAt: 0, version: book.version + 1 });
      picked = null;
    }
    const key = [s.calls, s.recordings, s.systems, book.version];
    if (!force && drawn && key.every((v, i) => v === drawn[i])) return;
    drawn = key;
    if (site) readTotal(site);
    chips.forEach((chip, i) => {
      const on = FILTERS[i][0] === filter;
      setClass(chip, 'on', on);
      chip.setAttribute('aria-pressed', String(on));
    });
    const v = s.calls || { open: [], recent: [] };
    // The pushed calls replace what was read of them (they learn their recordings and alerts).
    for (const x of v.recent) if (x.site === site) book.closed.set(callKey(x), x);
    const openKeys = new Set(v.open.map(callKey));
    const closed = [...book.closed.values()].filter(x => !openKeys.has(callKey(x))).sort((a, b) => b.started_unix_ms - a.started_unix_ms);
    const all = [...v.open, ...closed];
    const recs = new Map(((s.recordings && s.recordings.items) || []).map(r => [r.id, r]));
    const q = search.value.trim().toLowerCase();
    const items = all.filter(x => (filter !== 'followed' || !x.not_followed)
      && (filter !== 'alerts' || alertsOf(x, recordingOf(x, recs)).length > 0)
      && (!q || [x.tg, x.tg_name, x.source, x.source_name].some(f => known(f) && String(f).toLowerCase().includes(q))));
    const storage = s.recordings && s.recordings.storage;
    const total = book.total === null ? null : Math.max(book.total, closed.length);
    const recorded = s.recordings ? s.recordings.total : null;
    setText(count, s.calls ? [
      total === null ? `${num(all.length)} calls` : `${num(all.length)} of ${num(total)} calls`,
      known(recorded) ? `${num(recorded)} recorded` : '',
      storage && !storage.enabled ? 'recording off' : '',
    ].filter(Boolean).join(' · ') : '');
    count.title = recordingNote(storage);
    const lv = s.status.live;
    const sys = lv && lv.state === 'live' ? (s.systems || []).find(x => x.id === lv.system.id) : null;
    const ctx = { now: boardNow(), clockValid: !!(s.status.clock && s.status.clock.valid), recs, site, sys, picked };
    keyedList(rows, items, callKey, () => createRow(pick), (el, x) => updateRow(el, x, ctx));
    empty.hidden = items.length > 0;
    setText(empty, !s.calls ? 'Loading…' : q || filter !== 'all' || all.length ? 'No matching calls.' : lv && lv.state === 'live' ? 'No calls yet.' : 'No site is live.');
    more.hidden = !site;
    moreBtn.hidden = book.done || book.loading;
    setText(moreBtn, filtering() ? 'Search older calls' : 'Load older calls');
    setText(moreText, book.error ? `Older calls not read: ${book.error}` : book.loading ? 'Reading older calls…' : book.done ? `All ${num(all.length)} calls of this site.` : '');
    // The picked call, whether or not the filter lists it.
    const x = picked ? all.find(c => callKey(c) === picked) : null;
    if (picked && !x) picked = null;
    detail.show(x || null, ctx, playAsked);
    playAsked = false;
  }

  const ages = setInterval(() => draw(s, true), AGES_MS);
  return {
    list,
    detail: detail.el,
    draw,
    stop() {
      clearInterval(ages);
      io.disconnect();
      detail.stop();
    },
  };
}
