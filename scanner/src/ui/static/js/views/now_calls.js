// Now's calls: every call of the live site, newest first (those on the air, then every closed
// one; the radio pushes the newest, older ones are read from the history as the list reaches its
// end), and the picked call beside the list: its recording's player with its alert tones as
// bookmarks, and what else is known of it. One recording plays at a time, in that panel, so the
// list can move under it.

import { h, toast, setText, setClass, keyedList, icon, iconButton } from '../dom.js';
import { api } from '../api.js';
import { mhz, ago, num, dur, clock, bytes, unitLabel, DASH, NOT_FOLLOWED, CLOSE_REASON } from '../format.js';
import { recordingPlayer } from '../audio/recording.js';
import { aliasOf, tgLabel, nameOf, colorOf } from '../aliases.js';

// The call list's ages move between pushes.
const AGES_MS = 5000;
// Calls read from the history a page at a time as the list is scrolled to its end.
const PAGE = 100;
// The follower serves the left speaker's talkgroups on lane 1 and the right's on lane 2.
export const SIDE = { 1: 'Left', 2: 'Right' };
const FILTERS = [['all', 'All', 'Every call'], ['followed', 'Followed', 'Only the calls a traffic channel followed'], ['alerts', 'Alerts', 'Only the calls with an alert tone']];

// Kept while the page is left for another.
let filter = 'all';

const known = v => v !== null && v !== undefined;
export const boardNow = s => s.status.now_unix_ms + (Date.now() - s.statusAt);

// The channel as the control channel names it ("ch 1-117", "LCN 6 TS2": the name carries its
// timeslot).
export function channel(x) {
  if (!x.channel) return '';
  return /^\d/.test(x.channel) ? `ch ${x.channel}` : x.channel;
}

// "warble 806.5/1506.0 Hz" (an alert, or a recording's bookmark).
export function alertLabel(a) {
  return a.label || `${a.kind.replace('_', '-')} ${a.tones_hz.map(t => t.toFixed(1)).join('/')} Hz`;
}

const callKey = x => `${x.site}|${x.call}|${x.started_unix_ms}`;
// Its recording: the one the call carries (stored calls, closed ones once saved), else the
// newest recordings' (pushed).
const recordingOf = (x, recs) => x.recording || recs.get(x.call) || null;
// Its alert tones: the call's, else its recording's bookmarks.
const alertsOf = (x, rec) => (x.alerts && x.alerts.length ? x.alerts : (rec && rec.bookmarks) || []);

function recordingNote(st) {
  if (!st) return '';
  if (!st.enabled) return 'Recording is off (Settings).';
  const where = st.store === 'sd' ? 'SD card' : 'RAM';
  const state = st.sd_status.state;
  const sd = state === 'ok' || st.store !== 'sd' ? '' : state === 'failing' ? ' (SD card writes failing: saving to RAM until one lands)' : ` (SD card ${state.replace('_', ' ')}: saving to RAM)`;
  return `Recording to ${where}${sd}: ${num(st.sd.count + st.ram.count)} kept, ${bytes(st.sd.bytes + st.ram.bytes)}.`;
}

// Locked recordings are kept for the locked days (Settings). The button changes in place, so
// the recording goes on playing.
function lockButton(r) {
  const label = r.locked ? 'Unlock: kept for the normal days again' : 'Lock: kept for the locked days';
  const b = iconButton(r.locked ? 'lock' : 'unlock', label, { cls: r.locked ? 'locked' : '' });
  b.addEventListener('click', async () => {
    b.disabled = true;
    try {
      const done = await api.lockRecording(r.id, !r.locked);
      Object.assign(r, done);
      toast(done.locked ? 'Locked' : 'Unlocked');
      b.replaceWith(lockButton(r));
    } catch (e) {
      toast(e.message, true);
      b.disabled = false;
    }
  });
  return b;
}

// What is known of a call beyond the panel's heading: who, when, its voice and vocoder counts,
// its channel time, its channel and its recording.
function detailRows(x, ctx, rec) {
  const others = (x.sources || []).filter(u => u !== x.source);
  const rows = [
    ['Call', `#${x.call}${x.site !== ctx.site ? ` (${x.site})` : ''}`],
    ['Granted', clock(x.started_unix_ms, ctx.clockValid) || ago(ctx.now - x.started_unix_ms)],
    ['Radio', unitLabel(x.source, x.source_name)],
  ];
  if (others.length) rows.push(['Also heard', others.join(', ')]);
  rows.push(['First voice', known(x.first_voice_unix_ms) ? `${num(x.first_voice_unix_ms - x.started_unix_ms)} ms after the grant` : DASH]);
  const errors = known(x.frame_errors) ? `, ${num(x.frame_errors)} with errors` : '';
  rows.push(['Voice', x.voice_frames ? `${dur(x.voice_frames * 20)} (${num(x.voice_frames)} frames${errors}${x.codec ? `, ${x.codec.toUpperCase()}` : ''})` : DASH]);
  if (rec && rec.voice) rows.push(['Vocoder', `${num(rec.voice.frames)} frames, ${num(rec.voice.errors)} errors, ${num(rec.voice.silent)} silent`]);
  rows.push(['On air', known(x.grant_ms) ? `${dur(x.grant_ms)} (the grant to its last update)` : DASH]);
  if (x.ended_unix_ms) {
    const why = x.close ? CLOSE_REASON[x.close] || x.close.replace(/_/g, ' ') : '';
    rows.push([x.not_followed ? 'Channel time' : 'Held open', `${dur(x.open_ms)}${why ? ` (${why})` : ''}`]);
  }
  if (x.end_lc) rows.push(['End', x.end_lc.replace(/_/g, ' ')]);
  rows.push(['Channel', [mhz(x.freq_hz), channel(x), known(x.slot) ? `slot ${x.slot}` : ''].filter(Boolean).join(' · ')]);
  if (x.not_followed) rows.push(['Not followed', NOT_FOLLOWED[x.not_followed] || x.not_followed]);
  const flags = [x.emergency && 'emergency', x.private && 'unit to unit', x.encrypted && 'encrypted'].filter(Boolean);
  if (flags.length) rows.push(['Flags', flags.join(', ')]);
  rows.push(['Recording', rec ? `${rec.file} (${bytes(rec.bytes)}, ${rec.store === 'sd' ? 'SD card' : 'RAM'})` : x.ended_unix_ms ? 'none' : DASH]);
  return rows;
}

// The picked call's panel. `show(x, ctx, play)` draws call `x` (null: none picked), and starts
// its recording when `play`.
function detailPanel(onClose) {
  const kicker = h('div', { class: 'kicker' });
  const title = h('div', { class: 'detail-title' });
  const sub = h('div', { class: 'dim' });
  const player = h('div', { class: 'detail-player' });
  const tools = h('div', { class: 'row' });
  const alerts = h('div', { class: 'detail-alerts' });
  const facts = h('dl', { class: 'kv' });
  const body = h('div', { class: 'detail-body', hidden: true },
    h('div', { class: 'detail-head' }, h('div', null, kicker, title, sub), iconButton('close', 'Close', { onclick: onClose })),
    player, tools, alerts, facts);
  const empty = h('div', { class: 'detail-empty', text: 'Pick a call to play its recording and see what is known of it.' });
  const el = h('section', { class: 'card now-detail', 'aria-label': 'Picked call' }, empty, body);
  // The recording in the player: { id, p }.
  let rec = null;
  let drawnTools = null;
  let drawnAlerts = null;
  let drawnFacts = null;

  function dropPlayer() {
    if (rec) rec.p.pause();
    rec = null;
    // Its bookmark buttons go with it.
    drawnTools = null;
    player.replaceChildren();
  }

  function show(x, ctx, play) {
    body.hidden = !x;
    empty.hidden = !!x;
    // With no call picked the panel gives way where it has no column of its own.
    setClass(el, 'empty', !x);
    if (!x) {
      dropPlayer();
      return;
    }
    const r = recordingOf(x, ctx.recs);
    const marks = alertsOf(x, r);
    setText(kicker, x.ended_unix_ms ? `Call · ${clock(x.started_unix_ms, ctx.clockValid) || ago(ctx.now - x.started_unix_ms)}` : 'On the air');
    setText(title, tgLabel(x.tg, x.tg_name));
    setText(sub, [
      nameOf(x.tg, x.tg_name) ? `TG ${x.tg}` : '',
      `from ${unitLabel(x.source, x.source_name)}`,
      x.lane ? `Traffic ${x.lane}, ${(SIDE[x.lane] || '').toLowerCase()}` : '',
    ].filter(Boolean).join(' · '));

    // The player stays while it is the same recording, so a redraw never stops it.
    if (!r || !rec || rec.id !== r.id) {
      dropPlayer();
      if (r) {
        rec = { id: r.id, p: recordingPlayer(`/api/v1/recordings/${r.id}.wav`) };
        player.replaceChildren(rec.p.el);
      } else {
        player.replaceChildren(h('span', { class: 'dim', text: !x.ended_unix_ms ? 'Its recording comes when the call closes.' : x.encrypted ? 'Encrypted: no recording.' : 'No recording.' }));
      }
    }
    if (play && rec) rec.p.play(null);

    const toolsKey = r ? `${r.id}|${marks.length}|${r.store}|${r.locked}` : '';
    if (toolsKey !== drawnTools) {
      drawnTools = toolsKey;
      const p = rec && rec.p;
      tools.replaceChildren(...(r ? [
        ...marks.filter(b => known(b.offset_ms)).map(b => h('button', {
          class: 'btn small mark', type: 'button', title: `Play from the ${alertLabel(b)}, ${dur(b.offset_ms)} in`,
          onclick: () => p.play(b.offset_ms / 1000),
        }, icon('bell'), dur(b.offset_ms))),
        h('div', { class: 'spacer' }),
        h('a', { class: 'btn small icon', href: `/api/v1/recordings/${r.id}.wav`, download: r.file, title: 'Download the WAV (its alert tones are cue points)', 'aria-label': 'Download the WAV' }, icon('download')),
        r.store === 'sd' ? lockButton(r) : null,
      ] : []));
    }
    const alertsKey = marks.map(a => `${alertLabel(a)}|${a.offset_ms}|${a.duration_ms}`).join(',');
    if (alertsKey !== drawnAlerts) {
      drawnAlerts = alertsKey;
      alerts.replaceChildren(...marks.map(a => h('div', { class: 'detail-alert' }, icon('bell'),
        h('span', { text: [alertLabel(a), known(a.offset_ms) ? `${dur(a.offset_ms)} in` : '', known(a.duration_ms) ? `for ${dur(a.duration_ms)}` : ''].filter(Boolean).join(', ') }))));
    }
    const rows = detailRows(x, ctx, r);
    const factsKey = JSON.stringify(rows);
    if (factsKey !== drawnFacts) {
      drawnFacts = factsKey;
      facts.replaceChildren(...rows.flatMap(([k, v]) => [h('dt', { text: k }), h('dd', { text: v })]));
    }
  }

  return { el, show, stop: dropPlayer };
}

function createRow(pick) {
  const c = {
    when: h('div', { class: 'when' }),
    dot: h('span', { class: 'tg-dot' }),
    tg: h('span', { class: 'tg' }),
    flag: h('span', { class: 'c-flag' }),
    tgSub: h('div', { class: 'sub' }),
    src: h('div', { class: 'ellipsis' }),
    srcSub: h('div', { class: 'sub' }),
    freq: h('div', { class: 'num' }),
    freqSub: h('div', { class: 'sub' }),
    end: h('div', { class: 'c-end' }),
  };
  const el = h('div', { class: 'call-row', tabindex: '0' },
    c.when,
    h('div', { class: 'c-tg' }, h('div', { class: 'tg-line' }, c.dot, c.tg, c.flag), c.tgSub),
    h('div', { class: 'c-src' }, c.src, c.srcSub),
    h('div', { class: 'c-freq' }, c.freq, c.freqSub),
    c.end);
  el._c = c;
  el.addEventListener('click', e => pick(el._x, !!e.target.closest('.c-play')));
  el.addEventListener('keydown', e => {
    if (e.target === el && (e.key === 'Enter' || e.key === ' ')) {
      e.preventDefault();
      pick(el._x, false);
    }
  });
  return el;
}

// The row's last cell: its recording's play button, or why there is none.
function drawEnd(c, x, rec, open) {
  const voice = x.voice_frames ? dur(x.voice_frames * 20) : '';
  let key;
  let make;
  if (rec) {
    key = `rec|${rec.id}|${rec.duration_ms}`;
    make = () => [h('button', { class: 'btn small c-play', type: 'button', title: 'Play the recording' }, icon('play'), dur(rec.duration_ms))];
  } else if (x.encrypted) {
    key = 'enc';
    make = () => [h('span', { class: 'c-note' }, icon('lock'), 'Encrypted')];
  } else {
    const text = x.not_followed ? NOT_FOLLOWED[x.not_followed] || x.not_followed : open ? (voice ? `voice ${voice}` : 'no voice yet') : voice || DASH;
    key = `text|${text}`;
    make = () => [h('span', { class: 'c-note', text })];
  }
  if (c.end.dataset.k === key) return;
  c.end.dataset.k = key;
  c.end.replaceChildren(...make());
}

function updateRow(el, x, ctx) {
  const c = el._c;
  el._x = x;
  const open = !x.ended_unix_ms;
  const rec = recordingOf(x, ctx.recs);
  const alerts = alertsOf(x, rec);
  setClass(el, 'nf', !!x.not_followed);
  setClass(el, 'open', open);
  setClass(el, 'sel', callKey(x) === ctx.picked);
  if (open) {
    if (!c.when.querySelector('.badge')) c.when.replaceChildren(h('span', { class: 'badge voice', text: 'On air' }));
  } else {
    setText(c.when, clock(x.started_unix_ms, ctx.clockValid) || ago(ctx.now - x.started_unix_ms));
  }
  c.dot.style.background = x.not_followed ? 'var(--idle)' : colorOf(x.tg, aliasOf(ctx.sys, x.tg));
  setText(c.tg, tgLabel(x.tg, x.tg_name));
  const flagKey = alerts.map(alertLabel).join(', ');
  if (c.flag.dataset.k !== flagKey) {
    c.flag.dataset.k = flagKey;
    c.flag.replaceChildren(...(alerts.length ? [h('span', { class: 'badge alert', text: 'Alert', title: flagKey })] : []));
  }
  setText(c.tgSub, [nameOf(x.tg, x.tg_name) ? `TG ${x.tg}` : '', x.private ? 'private' : '', x.emergency ? 'emergency' : ''].filter(Boolean).join(' · '));
  setText(c.src, x.source_name || (x.source ? String(x.source) : 'unknown'));
  setText(c.srcSub, x.source_name && x.source ? String(x.source) : '');
  setText(c.freq, mhz(x.freq_hz));
  setText(c.freqSub, [channel(x), x.lane ? (SIDE[x.lane] || '').toLowerCase() : ''].filter(Boolean).join(' · '));
  drawEnd(c, x, rec, open);
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
    if (oldest === Infinity) oldest = boardNow(s) + 1;
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
    const ctx = { now: boardNow(s), clockValid: !!(s.status.clock && s.status.clock.valid), recs, site, sys, picked };
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
