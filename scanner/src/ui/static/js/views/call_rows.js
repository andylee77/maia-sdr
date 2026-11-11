// A call as a list row, and the picked call's panel: its recording's player with its alert tones
// as bookmarks, and what else is known of it. Now's calls and Activity's share them. One recording
// plays at a time, in the panel, so a list can move under it.
//
// A page hands each draw a context: { now, clockValid (the board's clock), recs (recordings by
// id, for calls that do not carry theirs yet), site (the page's site), sys (its system, for the
// talkgroups' colours), picked (the picked call's key), when (optional: a call's time as text) }.

import { h, toast, setText, setClass, icon, iconButton } from '../dom.js';
import { api } from '../api.js';
import { mhz, ago, num, dur, clock, bytes, unitLabel, DASH, NOT_FOLLOWED, CLOSE_REASON } from '../format.js';
import { recordingPlayer } from '../audio/recording.js';
import { aliasOf, tgLabel, nameOf, colorOf } from '../aliases.js';

// The follower serves the left speaker's talkgroups on lane 1 and the right's on lane 2.
export const SIDE = { 1: 'Left', 2: 'Right' };

const known = v => v !== null && v !== undefined;

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

export const callKey = x => `${x.site}|${x.call}|${x.started_unix_ms}`;
// Its recording: the one the call carries (stored calls, closed ones once saved), else the
// newest recordings' (pushed).
export const recordingOf = (x, recs) => x.recording || recs.get(x.call) || null;
// Its alert tones: the call's, else its recording's bookmarks.
export const alertsOf = (x, rec) => (x.alerts && x.alerts.length ? x.alerts : (rec && rec.bookmarks) || []);

// When a call was granted: the page's own text, else the board's clock, or the call's age while
// that clock is not set.
const whenOf = (x, ctx) => (ctx.when ? ctx.when(x.started_unix_ms) : clock(x.started_unix_ms, ctx.clockValid) || ago(ctx.now - x.started_unix_ms));

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
    ['Granted', whenOf(x, ctx)],
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

// The picked call's panel. `show(x, ctx, play)` draws call `x` (null: none picked); `play` true
// starts its recording, a number starts it that many seconds in.
export function detailPanel(onClose, emptyText = 'Pick a call to play its recording and see what is known of it.') {
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
  const empty = h('div', { class: 'detail-empty', text: emptyText });
  const el = h('section', { class: 'card call-detail', 'aria-label': 'Picked call' }, empty, body);
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
    setText(kicker, x.ended_unix_ms ? `Call · ${whenOf(x, ctx)}` : 'On the air');
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
    if (rec && (play === true || typeof play === 'number')) rec.p.play(play === true ? null : play);

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

// A row for the keyed list; `pick(call, play)` when it is clicked (`play`: on its play button).
export function createRow(pick) {
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

export function updateRow(el, x, ctx) {
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
    setText(c.when, whenOf(x, ctx));
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
