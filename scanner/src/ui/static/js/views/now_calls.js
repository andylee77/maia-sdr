// Now's calls: every call of the live site, newest first, and the picked call's recording in a
// panel docked under the list (call_rows.js draws the rows and the panel). The calls on the air
// are drawn at each push; the closed ones only when one closed, learned its recording or was
// read from the history. The radio pushes the newest; older ones are read from the history as
// the list reaches its end, and only so many are kept, so a page left open does not grow.

import { h, setText, setClass, keyedList } from '../dom.js';
import { api } from '../api.js';
import { boardNow } from '../store.js';
import { num } from '../format.js';
import { callKey, recordingOf, alertsOf, detailPanel, createRow, updateRow } from './call_rows.js';

// The list's ages move between pushes while the board's clock is not set.
const AGES_MS = 5000;
// Calls read from the history a page at a time as the list is scrolled to its end.
const PAGE = 100;
// Closed calls kept: this many, and a page more for each page read, up to the most.
const KEEP = 300;
const KEEP_MOST = 2000;
const FILTERS = [['all', 'All', 'Every call'], ['heard', 'Heard', 'Only the calls a tuner followed'], ['alerts', 'Alerts', 'Only the calls with an alert tone']];

// Kept while the page is left for another.
let filter = 'all';

const known = v => v !== null && v !== undefined;

// The calls and the picked call's panel: `el` to show, `draw(store)` at each push, `play(call)`
// to pick a call and play its recording, `stop()`.
export function callsPane() {
  const count = h('span', { class: 'dim' });
  const search = h('input', { class: 'input', type: 'search', placeholder: 'Talkgroup or radio', 'aria-label': 'Filter the calls' });
  const chips = FILTERS.map(([value, text, title]) => h('button', { class: 'chip', type: 'button', text, title, onclick: () => {
    filter = value;
    draw(s, true);
  } }));
  const onAir = h('div', { class: 'calls' });
  const closedRows = h('div', { class: 'calls' });
  const empty = h('div', { class: 'calls-empty' });
  const moreBtn = h('button', { class: 'btn small', type: 'button', text: 'Load older calls' });
  const moreText = h('span');
  const more = h('div', { class: 'calls-more' }, moreText, moreBtn);
  let s = null;
  // What the closed rows were last drawn from.
  let drawn = [];
  // The picked call's key, and a play asked for with the pick.
  let picked = null;
  let playAsked = false;
  const detail = detailPanel(() => {
    picked = null;
    draw(s, true);
  }, '', { compact: true });
  const el = h('section', { class: 'card flush now-calls', 'aria-label': 'Calls' },
    h('div', { class: 'card-head' }, h('h2', { text: 'Calls' }), count, h('div', { class: 'spacer' }), h('div', { class: 'row', role: 'group', 'aria-label': 'Which calls' }, ...chips), search),
    h('div', { class: 'card-body' }, onAir, closedRows, empty, more),
    detail.el);
  const pick = (x, play) => {
    picked = callKey(x);
    playAsked = play;
    draw(s, true);
  };
  // This site's closed calls by key, how many are kept, and the history's paging.
  const book = { site: null, closed: new Map(), keep: KEEP, done: false, loading: false, error: null, version: 0 };
  // The closed calls as last listed, newest first.
  let closed = [];
  // The newest recordings by id, and the store's part they were taken from.
  let recs = new Map();
  let recsOf = null;
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
    if (!site || book.loading || book.done || book.closed.size >= KEEP_MOST) return;
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
      book.keep = Math.min(KEEP_MOST, book.closed.size + PAGE);
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

  // The pushed calls join the kept ones (they learn their recordings and alerts), and the oldest
  // past what is kept go.
  function keep(recent, site) {
    for (const x of recent) if (x.site === site) book.closed.set(callKey(x), x);
    if (book.closed.size <= book.keep) return;
    const byAge = [...book.closed.entries()].sort((a, b) => b[1].started_unix_ms - a[1].started_unix_ms);
    book.closed = new Map(byAge.slice(0, book.keep));
    book.done = false;
  }

  function draw(store, force) {
    s = store;
    if (!s || !s.status) return;
    const site = liveSite();
    if (site !== book.site) {
      Object.assign(book, { site, closed: new Map(), keep: KEEP, done: false, error: null, version: book.version + 1 });
      picked = null;
    }
    const v = s.calls || { open: [], recent: [] };
    if (s.recordings !== recsOf) {
      recsOf = s.recordings;
      recs = new Map(((s.recordings && s.recordings.items) || []).map(r => [r.id, r]));
    }
    const lv = s.status.live;
    const sys = lv && lv.state === 'live' ? (s.systems || []).find(x => x.id === lv.system.id) : null;
    const clockValid = !!(s.status.clock && s.status.clock.valid);
    const ctx = { now: boardNow(), clockValid, recs, site, sys, picked };
    const q = search.value.trim().toLowerCase();
    const shows = x => (filter !== 'heard' || !x.not_followed)
      && (filter !== 'alerts' || alertsOf(x, recordingOf(x, recs)).length > 0)
      && (!q || [x.tg, x.tg_name, x.source, x.source_name].some(f => known(f) && String(f).toLowerCase().includes(q)));

    // On the air: at each push.
    const open = v.open.filter(shows);
    keyedList(onAir, open, callKey, () => createRow(pick), (row, x) => updateRow(row, x, ctx));
    onAir.hidden = !open.length;

    // The closed ones: when what they are drawn from changed.
    const from = [v.recent, s.recordings, s.systems, book.version, filter, q, picked, v.open.map(callKey).join(), clockValid];
    if (force || from.some((x, i) => x !== drawn[i])) {
      drawn = from;
      keep(v.recent, site);
      const openKeys = new Set(v.open.map(callKey));
      closed = [...book.closed.values()].filter(x => !openKeys.has(callKey(x))).sort((a, b) => b.started_unix_ms - a.started_unix_ms);
      const items = closed.filter(shows);
      keyedList(closedRows, items, callKey, () => createRow(pick), (row, x) => updateRow(row, x, ctx));
      empty.hidden = items.length + open.length > 0;
      setText(empty, !s.calls ? 'Loading…' : q || filter !== 'all' || closed.length ? 'No matching calls.' : site ? 'No calls yet.' : 'No site is live.');
      const full = book.closed.size >= KEEP_MOST;
      more.hidden = !site;
      moreBtn.hidden = book.done || book.loading || full;
      setText(moreBtn, filtering() ? 'Search older calls' : 'Load older calls');
      setText(moreText, book.error ? `Older calls not read: ${book.error}`
        : book.loading ? 'Reading older calls…'
          : book.done ? (closed.length ? `All ${num(closed.length)} calls of this site.` : '')
            : full ? `The newest ${num(closed.length)} calls. Older ones are on Activity.` : '');
    }
    chips.forEach((chip, i) => {
      const on = FILTERS[i][0] === filter;
      setClass(chip, 'on', on);
      chip.setAttribute('aria-pressed', String(on));
    });
    setText(count, s.calls ? [v.open.length ? `${num(v.open.length)} on the air` : '', `${num(closed.length)} listed`].filter(Boolean).join(' · ') : '');
    // The picked call, whether or not the filter lists it.
    const x = picked ? v.open.find(c => callKey(c) === picked) || book.closed.get(picked) || null : null;
    if (picked && !x) picked = null;
    detail.show(x, ctx, playAsked);
    playAsked = false;
  }

  const ages = setInterval(() => {
    if (s && s.status && !(s.status.clock && s.status.clock.valid)) draw(s, true);
  }, AGES_MS);
  return {
    el,
    draw,
    play: x => pick(x, true),
    stop() {
      clearInterval(ages);
      io.disconnect();
      detail.stop();
    },
  };
}
