// Activity: a site's history, filling the screen. On the left who talks: the talkgroups or the
// radios by their time on the air; one picked narrows the page to it and shows what is known of
// it. In the middle the period's totals with the airtime per hour or day, and under them the
// calls, the alert tones heard (consoles' warbles and beeps, two-tone pages) or the packet data.
// On the right the picked call (call_rows.js). It shows the last 24 hours, 7 or 30 days, or the
// hours between two dates.
//
// Two kinds of time are shown apart. Voice is decoded on the voice channel (followed calls) and
// is measured. Grant time is all that is known of a call that was not followed (encrypted, or no
// lane free): from the grant to its last update on the control channel. It includes hang time and
// any other radio keying up on the grant, and is credited to the radio granted.

import { h, setText, setClass, toast, table, icon, iconButton, keyedList } from '../dom.js';
import { api } from '../api.js';
import { packetDataPane } from './packet_data.js';
import { dur, num, bytes, unitLabel, DASH, dayTime as when } from '../format.js';
import { tgLabel, nameOf } from '../aliases.js';
import { callKey, detailPanel, createRow, updateRow } from './call_rows.js';

// refresh: how often a period is reloaded at most (longer ones cost the radio more).
const PERIODS = [
  { key: '24h', label: '24 h', hours: 24, bucket: 'hour', refresh: 30000 },
  { key: '7d', label: '7 days', hours: 168, bucket: 'hour', refresh: 120000 },
  { key: '30d', label: '30 days', hours: 720, bucket: 'day', refresh: 300000 },
];
const HOUR = 3600000;
// Picked dates up to this long are drawn per hour, longer ones per day.
const HOURLY_MAX = 168 * HOUR;
// A list shows its newest rows: this many, twice as many each time more are asked for, at most LIST_MAX.
const LIST_ROWS = 50;
const LIST_MAX = 800;
// The talkgroups or radios listed, busiest first; any other is reached by typing its number.
const WHO_ROWS = 200;
const WHO = [['tg', 'Talkgroups', 'Talkgroup'], ['unit', 'Radios', 'Radio']];
const TABS = [['calls', 'Calls'], ['alerts', 'Alerts'], ['data', 'Packet data']];
const ALERT_NOTE = 'Found in the decoded audio of followed calls. The vocoder rebuilds a tone to within about 1 % of what '
  + 'was sent, so the same tones read a little differently from call to call. The dispatch is what the alert announced: '
  + 'the next transmission of the same radio on the talkgroup with 4 s of voice, each of its key-ups within 10 s of its last '
  + '(another radio\'s answer between is passed over); without one, the longest of them with 3 s.';
const GRANT_NOTE = 'Encrypted and not-followed calls show grant time: from the grant to its last update on the control channel. '
  + 'It includes hang time and any other radio that keyed up on the grant, and is credited to the radio granted. '
  + 'Voice is decoded on the voice channel.';

const EST_NOTE = 'Encrypted grant time x the decoded voice per grant second of the period\'s followed calls';

// Kept while the page is left for another.
let who = 'tg';
let tab = 'calls';

function secs(s) {
  return s ? dur(s * 1000) : DASH;
}

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || '#888';
}

function metric(label, value, title) {
  const v = h('span', { class: 'm-value', text: value });
  return { el: h('div', { class: 'metric', title: title || '' }, h('span', { class: 'm-label', text: label }), v), v };
}

// "warble", "two-tone".
const kindLabel = kind => kind.replace('_', '-');
const tonesLabel = tones => tones.map(t => t.toFixed(1)).join(' / ') + ' Hz';

// "07:00:12" today, "10/2 07:00:12" another day.
function stamp(ms) {
  const d = new Date(ms);
  const p = n => String(n).padStart(2, '0');
  const time = `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
  return d.toDateString() === new Date().toDateString() ? time : `${d.getMonth() + 1}/${d.getDate()} ${time}`;
}

// A time as a date-and-hour input's value, and back, in this browser's time zone. The history
// counts by the hour, so a picked range runs on whole hours: minutes are dropped.
function hourValue(ms) {
  const d = new Date(ms);
  const p = n => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:00`;
}

function hourOf(value) {
  const d = new Date(value);
  if (Number.isNaN(d.getTime())) return null;
  d.setMinutes(0, 0, 0);
  return d.getTime();
}

// An alert's row: the same alert with the same dispatch keeps its row.
function alertKey(a) {
  const d = a.dispatch;
  return [a.site, a.call_id, a.at_ms, a.recorded ? 1 : 0, d ? `${d.call_id}:${d.recorded ? 1 : 0}` : ''].join('|');
}

// What an alert's row plays: the call that carried what it announced (from where its speech
// starts), else the alert's own call from its tone; `rec` when that call's recording is kept.
function alertPlays(a) {
  const d = a.dispatch;
  if (d && d.recorded) return { call: d.call_id, started: d.started_ms, at: d.offset_ms, rec: true, dispatch: true };
  if (a.recorded) return { call: a.call_id, started: a.call_started_ms, at: a.offset_ms, rec: true, dispatch: false };
  return d ? { call: d.call_id, started: d.started_ms, at: 0, rec: false, dispatch: true } : { call: a.call_id, started: a.call_started_ms, at: 0, rec: false, dispatch: false };
}

function alertRow(a, pick) {
  const d = a.dispatch;
  const p = alertPlays(a);
  const end = p.rec
    ? h('button', {
      class: 'btn small c-play', type: 'button',
      title: p.dispatch ? `Play the dispatch: ${d.call_id === a.call_id ? 'after the tone in the same call' : `call #${d.call_id}`}, ${dur(d.voice_ms)}` : `No dispatch found: play the alert tone (call #${a.call_id})`,
    }, icon('play'), p.dispatch ? dur(d.voice_ms) : 'Tone')
    : h('span', { class: 'c-note', text: 'not recorded' });
  const el = h('div', { class: 'call-row alert-row', tabindex: '0' },
    h('div', { class: 'when', text: stamp(a.at_ms) }),
    h('div', { class: 'c-tg' },
      h('div', { class: 'tg', text: tgLabel(a.tg, a.tg_name) }),
      h('div', { class: 'sub', text: `from ${unitLabel(a.source, a.source_name)}` })),
    h('div', { class: 'c-src' },
      h('div', null, h('span', { class: 'badge alert', text: kindLabel(a.kind) })),
      h('div', { class: 'sub', text: tonesLabel(a.tones_hz) })),
    h('div', { class: 'c-freq', title: d ? `Its dispatch: call #${d.call_id}` : 'No transmission of 3 s from the same radio followed, each key-up within 10 s' },
      h('div', { class: 'num', text: d ? dur(d.voice_ms) : DASH }), h('div', { class: 'sub', text: 'dispatch' })),
    h('div', { class: 'c-end' }, end));
  el.addEventListener('click', e => pick(a, !!e.target.closest('.c-play')));
  el.addEventListener('keydown', e => {
    if (e.target === el && (e.key === 'Enter' || e.key === ' ')) {
      e.preventDefault();
      pick(a, false);
    }
  });
  return el;
}

// Under a list of the newest rows: how many it shows, and a button for more. `total` is null
// when only the rows read are known; nothing is shown under a list that has them all.
function moreRow(shown, total, rows, what, onMore) {
  if (total === null ? shown < rows : shown >= total) return null;
  const count = total === null ? `The newest ${num(shown)} ${what}.` : `The newest ${num(shown)} of ${num(total)} ${what}.`;
  if (rows >= LIST_MAX) return h('div', { class: 'calls-more' }, h('span', { text: `${count} Pick dates to see the others.` }));
  const btn = h('button', { class: 'btn small', type: 'button', text: 'Show more' });
  btn.addEventListener('click', () => onMore(Math.min(LIST_MAX, rows * 2)));
  return h('div', { class: 'calls-more' }, h('span', { text: count }), btn);
}

// A talkgroup or a radio as a row of the left column: its name, its time on the air (voice, then
// grant time) as a bar against the busiest row's (`max` seconds), and what else is counted.
function whoRow({ name, sub, voice_s, grant_s, calls, encrypted, max, title, onClick }) {
  const total = voice_s + grant_s;
  // Grant time is the encrypted calls' when every call is encrypted, the clear ones' when none
  // is; the history does not split it per row otherwise.
  const grant = !grant_s ? null : encrypted >= calls ? ['e', 'Encrypted (grant)'] : encrypted ? ['m', 'Grant time, clear and encrypted'] : ['g', 'Clear, not followed (grant)'];
  const width = v => `${max > 0 ? Math.max(0.5, (v / max) * 100) : 0}%`;
  return h('button', { class: 'who-row', type: 'button', title, onclick: onClick },
    h('span', { class: 'who-name', text: name }),
    h('span', { class: 'who-time', text: secs(total) }),
    h('span', { class: 'sub', text: sub }),
    h('span', { class: 'who-bar' },
      voice_s ? h('span', { class: 'v', style: { width: width(voice_s) }, title: `Voice ${secs(voice_s)}` }) : null,
      grant ? h('span', { class: grant[0], style: { width: width(grant_s) }, title: `${grant[1]} ${secs(grant_s)}` }) : null));
}

const plural = (n, one, many) => `${num(n)} ${n === 1 ? one : many}`;

// Stacked bars: voice, clear grant-only, encrypted grant-only, per bucket.
function chart() {
  const canvas = h('canvas', { class: 'act-plot', 'aria-label': 'Airtime per bucket' });
  const readout = h('span', { class: 'act-readout', text: ' ' });
  const legend = h('div', { class: 'act-legend' });
  let data = null;
  function key(color, text) {
    return h('span', { class: 'act-key' }, h('span', { class: 'act-swatch', style: { background: color } }), h('span', { text }));
  }
  function draw() {
    if (!data) return;
    const { buckets, bucketMs } = data;
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth || 600;
    const hgt = canvas.clientHeight || 160;
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(hgt * dpr);
    const g = canvas.getContext('2d');
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, w, hgt);
    const colors = { voice: cssVar('--voice'), clear: cssVar('--acquire'), enc: cssVar('--danger'), line: cssVar('--line'), text: cssVar('--text-faint') };
    legend.replaceChildren(key(colors.voice, 'Voice (decoded)'), key(colors.clear, 'Clear, not followed (grant)'), key(colors.enc, 'Encrypted (grant)'));
    const left = 44, bottom = 22, top = 8, right = 6;
    const pw = w - left - right, ph = hgt - top - bottom;
    const total = b => b.voice_s + b.clear_grant_s + b.encrypted_grant_s;
    const max = Math.max(60, ...buckets.map(total));
    // Minutes on the y axis, rounded to a tidy step.
    const maxMin = max / 60;
    const step = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600].find(s => maxMin / s <= 4) || 1200;
    const topMin = Math.ceil(maxMin / step) * step;
    g.font = '11px system-ui, sans-serif';
    g.fillStyle = colors.text;
    g.strokeStyle = colors.line;
    g.lineWidth = 1;
    g.textAlign = 'right';
    g.textBaseline = 'middle';
    for (let m = 0; m <= topMin; m += step) {
      const y = top + ph - (m / topMin) * ph;
      g.beginPath(); g.moveTo(left, Math.round(y) + 0.5); g.lineTo(w - right, Math.round(y) + 0.5); g.stroke();
      g.fillText(m >= 60 && m % 60 === 0 ? (m / 60) + ' h' : m + ' m', left - 6, y);
    }
    const n = buckets.length;
    const bw = pw / Math.max(1, n);
    const gap = bw > 6 ? 1 : 0;
    buckets.forEach((b, i) => {
      let y = top + ph;
      const x = left + i * bw;
      for (const [v, c] of [[b.voice_s, colors.voice], [b.clear_grant_s, colors.clear], [b.encrypted_grant_s, colors.enc]]) {
        if (!v) continue;
        const bh = (v / 60 / topMin) * ph;
        g.fillStyle = c;
        g.fillRect(x + gap, y - bh, Math.max(1, bw - 2 * gap), bh);
        y -= bh;
      }
    });
    // X labels: about eight, at bucket starts.
    g.fillStyle = colors.text;
    g.textAlign = 'center';
    g.textBaseline = 'top';
    const every = Math.max(1, Math.ceil(n / 8));
    for (let i = 0; i < n; i += every) {
      // A day by its midday: a day's bucket starts an hour off midnight across a clock change.
      const d = new Date(buckets[i].t + (bucketMs >= 86400000 ? bucketMs / 2 : 0));
      const label = bucketMs >= 86400000 ? (d.getMonth() + 1) + '/' + d.getDate()
        : (d.getHours() === 0 ? (d.getMonth() + 1) + '/' + d.getDate() : String(d.getHours()).padStart(2, '0') + ':00');
      g.fillText(label, left + i * bw + bw / 2, top + ph + 5);
    }
  }
  canvas.addEventListener('mousemove', e => {
    if (!data || !data.buckets.length) return;
    const r = canvas.getBoundingClientRect();
    const left = 44, right = 6;
    const i = Math.floor(((e.clientX - r.left - left) / (r.width - left - right)) * data.buckets.length);
    const b = data.buckets[i];
    if (!b) return;
    const d = new Date(b.t + (data.bucketMs >= 86400000 ? data.bucketMs / 2 : 0));
    const at = data.bucketMs >= 86400000 ? d.toLocaleDateString() : d.toLocaleDateString() + ' ' + String(d.getHours()).padStart(2, '0') + ':00';
    setText(readout, at + ': ' + b.calls + ' call' + (b.calls === 1 ? '' : 's') + (b.encrypted ? ' (' + b.encrypted + ' encrypted)' : '')
      + '; voice ' + secs(b.voice_s) + ', clear grant ' + secs(b.clear_grant_s) + ', encrypted grant ' + secs(b.encrypted_grant_s) + '.');
  });
  canvas.addEventListener('mouseleave', () => setText(readout, ' '));
  window.addEventListener('resize', draw);
  return {
    el: h('div', null, canvas, h('div', { class: 'act-under' }, legend, readout)),
    update(buckets, bucketMs) { data = { buckets, bucketMs }; draw(); },
    destroy() { window.removeEventListener('resize', draw); },
  };
}

// A summary with no calls.
const NOTHING = { calls: 0, followed: 0, encrypted: 0, voice_s: 0, clear_grant_s: 0, encrypted_grant_s: 0, voice_per_grant: null, talkgroups: 0, radios: 0 };

// A card's note behind its "i" button.
function noteToggle(text, label) {
  const note = h('p', { class: 'card-note', text, hidden: true });
  const btn = iconButton('info', label, { onclick: () => { note.hidden = !note.hidden; } });
  return { btn, note };
}

export function mount(host) {
  // `range` ({ from, to }, unix ms on whole hours) is the picked dates; without it the page shows
  // the last `period`. `scope` ({ kind: 'tg' | 'unit', id, label }) is the talkgroup or radio the
  // page is narrowed to. `alertRows` and `callRows` are how many rows the two lists ask for.
  const st = { period: PERIODS[0], range: null, site: null, scope: null, alertRows: LIST_ROWS, callRows: LIST_ROWS };
  const tz = -new Date().getTimezoneOffset();
  const firstRows = () => { st.alertRows = LIST_ROWS; st.callRows = LIST_ROWS; };
  // The store, for the systems (a talkgroup's colour).
  let s = null;
  // The picked call: its key, the call, and the alert's row it was picked from.
  let picked = null;
  // Each pick's number, and the page left: an alert's call read from the history is shown only
  // while its pick is the newest and the page is there.
  let picks = 0;
  let gone = false;

  // The bar: the site, the period or the dates, and the export.
  const siteSel = h('select', { class: 'input pick', 'aria-label': 'Site' });
  const chips = PERIODS.map(p => {
    const b = h('button', { type: 'button', class: 'chip', text: p.label });
    b.addEventListener('click', () => { st.period = p; st.range = null; firstRows(); load(); });
    return [p, b];
  });
  const datesChip = h('button', {
    type: 'button', class: 'chip', text: 'Dates',
    title: 'Show the hours between two dates. Picked dates stay as they are: they are not reloaded as calls close.',
  });
  const fromIn = h('input', { class: 'input', type: 'datetime-local', step: '3600', 'aria-label': 'From' });
  const toIn = h('input', { class: 'input', type: 'datetime-local', step: '3600', 'aria-label': 'To' });
  const dates = h('div', { class: 'row', hidden: true },
    h('span', { class: 'dim', text: 'From' }), fromIn, h('span', { class: 'dim', text: 'to' }), toIn);
  const kept = h('span', { class: 'act-kept' });
  const csv = h('a', { class: 'btn small', href: '#', title: 'The calls of the period (of the picked talkgroup or radio) as a CSV file' }, icon('download'), 'CSV');
  const bar = h('section', { class: 'card act-bar', 'aria-label': 'Site and period' },
    siteSel, h('div', { class: 'row', role: 'group', 'aria-label': 'Period' }, ...chips.map(c => c[1]), datesChip), dates,
    h('div', { class: 'spacer' }), kept, csv);
  siteSel.addEventListener('change', () => { st.site = siteSel.value; st.scope = null; firstRows(); unpick(); load(); });
  // The dates start as the period shown, ending with the hour now running.
  datesChip.addEventListener('click', () => {
    if (st.range) return;
    const to = hourOf(Date.now()) + HOUR;
    st.range = { from: to - st.period.hours * HOUR, to };
    firstRows();
    load();
  });
  const pickDates = () => {
    const from = hourOf(fromIn.value);
    const to = hourOf(toIn.value);
    // A date still being typed has no value yet.
    if (from === null || to === null) return;
    if (from >= to) {
      toast('The dates end before they start.', true);
      return;
    }
    st.range = { from, to };
    firstRows();
    load();
  };
  fromIn.addEventListener('change', pickDates);
  toIn.addEventListener('change', pickDates);

  // Who: the talkgroups or the radios, or the one the page is narrowed to.
  const whoChips = WHO.map(([value, text]) => h('button', { class: 'chip', type: 'button', text, onclick: () => {
    who = value;
    whoSearch.value = '';
    load();
  } }));
  const whoSearch = h('input', { class: 'input', type: 'search', placeholder: 'Name or number', 'aria-label': 'Find a talkgroup or a radio' });
  const whoListHead = h('div', { class: 'card-head' }, h('div', { class: 'row', role: 'group', 'aria-label': 'Who' }, ...whoChips), h('div', { class: 'spacer' }), whoSearch);
  const scopeKicker = h('div', { class: 'kicker' });
  const scopeTitle = h('div', { class: 'who-title' });
  const whoScopeHead = h('div', { class: 'card-head', hidden: true },
    iconButton('back', 'Back to every talkgroup and radio', { onclick: () => setScope(null) }), h('div', { class: 'who-scope' }, scopeKicker, scopeTitle));
  const whoBody = h('div', { class: 'card-body' });
  const whoCard = h('section', { class: 'card flush act-who', 'aria-label': 'Talkgroups and radios' }, whoListHead, whoScopeHead, whoBody);
  // The rows read for the list, kept for the search.
  let whoItems = [];
  whoSearch.addEventListener('input', () => renderWho());
  whoSearch.addEventListener('keydown', e => {
    if (e.key !== 'Enter') return;
    const first = whoBody.querySelector('.who-row');
    if (first) first.click();
  });

  // The totals and the chart.
  const sumTitle = h('h2', { text: 'Summary' });
  const grant = noteToggle(GRANT_NOTE, 'What voice and grant time are');
  const m = {
    calls: metric('Calls', DASH),
    followed: metric('Followed', DASH),
    enc: metric('Encrypted', DASH),
    voice: metric('Voice', DASH, 'Decoded on the voice channel'),
    clear: metric('Clear grant', DASH, 'Grant time of clear calls no lane followed'),
    encTime: metric('Enc. grant', DASH, 'Grant time of encrypted calls (control channel)'),
    est: metric('Enc. est. voice', DASH, EST_NOTE),
    tgs: metric('Talkgroups', DASH),
    radios: metric('Radios', DASH),
  };
  // What the history counts only for a whole site.
  const SITE_ONLY = [m.followed, m.est, m.tgs, m.radios];
  const plot = chart();
  const sumCard = h('section', { class: 'card act-sum', 'aria-label': 'Summary' },
    h('div', { class: 'card-head' }, sumTitle, h('div', { class: 'spacer' }), grant.btn),
    h('div', { class: 'metrics' }, ...Object.values(m).map(x => x.el)),
    plot.el, grant.note);

  // The lists: the calls, the alerts, the packet data.
  const tabChips = TABS.map(([value, text]) => h('button', { class: 'chip', type: 'button', text, onclick: () => {
    tab = value;
    firstRows();
    load();
  } }));
  const listCount = h('span', { class: 'dim' });
  const alertNote = noteToggle(ALERT_NOTE, 'How an alert and its dispatch are found');
  const callList = h('div', { class: 'calls' });
  const callsEmpty = h('div', { class: 'calls-empty', hidden: true });
  const callsMore = h('div');
  const callsBox = h('div', null, callList, callsEmpty, callsMore);
  const alertGroups = h('div', { class: 'act-pad' });
  const alertList = h('div', { class: 'calls' });
  const alertsMore = h('div');
  const alertsBox = h('div', { hidden: true }, alertNote.note, alertGroups, alertList, alertsMore);
  const packets = packetDataPane();
  const dataBox = h('div', { class: 'act-pad', hidden: true }, packets.el);
  const listCard = h('section', { class: 'card flush act-list', 'aria-label': 'Calls, alerts and packet data' },
    h('div', { class: 'card-head' }, h('div', { class: 'row', role: 'group', 'aria-label': 'What to list' }, ...tabChips), listCount, h('div', { class: 'spacer' }), alertNote.btn),
    h('div', { class: 'card-body' }, callsBox, alertsBox, dataBox));

  const detail = detailPanel(() => unpick(), 'Pick a call or an alert to play its recording and see what is known of it.');

  host.append(h('div', { class: 'activity' }, bar, whoCard, sumCard, listCard, detail.el));

  // `st.site` is a site id, or `system:<id>` for every site of a system.
  function query(extra = {}) {
    const q = new URLSearchParams(st.range ? { from: String(st.range.from), to: String(st.range.to) } : { hours: String(st.period.hours) });
    if (st.site && st.site.startsWith('system:')) q.set('system', st.site.slice(7));
    else if (st.site) q.set('site', st.site);
    for (const [k, v] of Object.entries(extra)) if (v !== undefined && v !== null) q.set(k, String(v));
    return q.toString();
  }
  function scopeQuery() {
    if (!st.scope) return {};
    return st.scope.kind === 'tg' ? { tg: st.scope.id } : { unit: st.scope.id };
  }
  function setScope(scope) {
    st.scope = scope;
    firstRows();
    load();
  }
  const tgScope = (tg, alias) => ({ kind: 'tg', id: tg, label: tgLabel(tg, alias) });
  const unitScope = (unit, alias) => ({ kind: 'unit', id: unit, label: alias || `Radio ${unit}` });
  // The chart's buckets: the period's, or by the length of the picked dates, with days starting
  // at the midnight of the dates' first day.
  function buckets() {
    if (!st.range) return { bucket: st.period.bucket, tz };
    return { bucket: st.range.to - st.range.from <= HOURLY_MAX ? 'hour' : 'day', tz: -new Date(st.range.from).getTimezoneOffset() };
  }

  // What the shared rows and the panel are drawn with.
  function ctx() {
    const site = st.site && !st.site.startsWith('system:') ? st.site : null;
    const sys = ((s && s.systems) || []).find(x => (site ? x.sites.some(y => y.id === site) : st.site === `system:${x.id}`)) || null;
    return { now: Date.now(), clockValid: true, recs: new Map(), site, sys, picked: picked && picked.key, when: stamp };
  }
  // The picked call in its panel and marked in the lists; `play` as detailPanel takes it.
  function showPicked(play) {
    const c = ctx();
    for (const el of callList.children) if (el._x) setClass(el, 'sel', callKey(el._x) === c.picked);
    for (const el of alertList.children) setClass(el, 'sel', !!picked && el.dataset.key === picked.alert);
    detail.show(picked ? picked.x : null, c, play);
  }
  function unpick() {
    picks += 1;
    picked = null;
    showPicked(false);
  }
  function pickCall(x, play) {
    picks += 1;
    picked = { key: callKey(x), x, alert: null };
    showPicked(play);
  }
  // An alert's call is read from the history by its grant time.
  async function pickAlert(a, play) {
    picks += 1;
    const my = picks;
    const p = alertPlays(a);
    try {
      const q = new URLSearchParams({ site: a.site, tg: String(a.tg), from: String(p.started), to: String(p.started + 1), limit: '8' });
      const r = await api.activity('calls', q.toString());
      if (my !== picks || gone) return;
      const x = r.items.find(c => c.call === p.call);
      if (!x) throw new Error('its call is no longer in the history');
      picked = { key: callKey(x), x, alert: alertKey(a) };
      showPicked(play && p.rec ? p.at / 1000 : false);
    } catch (e) {
      if (my === picks && !gone) toast(`Alert: ${e.message}`, true);
    }
  }

  function renderSites(d) {
    const sites = d.sites.slice();
    if (d.active && !sites.some(x => x.site === d.active)) sites.unshift({ site: d.active, label: d.active, calls: 0 });
    if (!st.site) st.site = d.active || (sites[0] && sites[0].site) || null;
    // A system with several sites with history can be shown as a whole.
    const systems = new Map();
    for (const x of sites) if (x.system) systems.set(x.system, (systems.get(x.system) || 0) + 1);
    const whole = [...systems].filter(([, n]) => n > 1).map(([id]) =>
      h('option', { value: 'system:' + id, text: 'All sites of ' + id }));
    siteSel.replaceChildren(...sites.map(x => h('option', { value: x.site, text: (x.label || x.site) + (x.site === d.active ? ' (live)' : '') + ' · ' + num(x.calls) + ' calls' })), ...whole);
    siteSel.value = st.site;
    setText(kept, `History kept ${d.retention_days} days · ${bytes(d.used_bytes)} of ${bytes(d.max_bytes)} · ${d.on_sd ? 'SD card' : 'RAM, lost on reboot'}`);
    kept.title = d.database + (d.on_sd ? '' : ' (insert an SD card to keep it)');
    setClass(kept, 'warn', !d.on_sd);
  }

  // The site's totals, or with the page narrowed the totals of what the chart shows.
  function renderSummary(sum, series) {
    setText(sumTitle, st.scope ? st.scope.label : 'Summary');
    for (const x of SITE_ONLY) x.el.hidden = !!st.scope;
    const t = st.scope
      ? series.reduce((a, b) => ({
        calls: a.calls + b.calls, encrypted: a.encrypted + b.encrypted, voice_s: a.voice_s + b.voice_s,
        clear_grant_s: a.clear_grant_s + b.clear_grant_s, encrypted_grant_s: a.encrypted_grant_s + b.encrypted_grant_s,
      }), { calls: 0, encrypted: 0, voice_s: 0, clear_grant_s: 0, encrypted_grant_s: 0 })
      : sum;
    setText(m.calls.v, num(t.calls));
    setText(m.enc.v, num(t.encrypted));
    setText(m.voice.v, secs(t.voice_s));
    setText(m.clear.v, secs(t.clear_grant_s));
    setText(m.encTime.v, secs(t.encrypted_grant_s));
    if (st.scope) return t;
    setText(m.followed.v, num(sum.followed));
    const est = sum.voice_per_grant && sum.encrypted_grant_s;
    setText(m.est.v, est ? '≈ ' + secs(sum.encrypted_grant_s * sum.voice_per_grant) : DASH);
    m.est.el.title = EST_NOTE + (est ? ` (×${sum.voice_per_grant.toFixed(2)} in this period)` : '');
    setText(m.tgs.v, num(sum.talkgroups));
    setText(m.radios.v, num(sum.radios));
    return t;
  }

  const tgRow = (t, max) => whoRow({
    name: tgLabel(t.tg, t.alias),
    sub: [nameOf(t.tg, t.alias) ? `TG ${t.tg}` : '', plural(t.calls, 'call', 'calls'), t.radios !== undefined ? plural(t.radios, 'radio', 'radios') : '',
      t.encrypted ? `${num(t.encrypted)} encrypted` : '', `last ${when(t.last_ms)}`].filter(Boolean).join(' · '),
    voice_s: t.voice_s, grant_s: t.grant_s, calls: t.calls, encrypted: t.encrypted, max,
    title: 'Show this talkgroup', onClick: () => setScope(tgScope(t.tg, t.alias)),
  });
  const unitRow = (u, max) => whoRow({
    name: u.alias || String(u.unit),
    sub: [u.alias ? String(u.unit) : '', plural(u.calls, 'call', 'calls'), u.talkgroups !== undefined ? plural(u.talkgroups, 'talkgroup', 'talkgroups') : '',
      u.encrypted ? `${num(u.encrypted)} encrypted` : '', `last ${when(u.last_ms)}`].filter(Boolean).join(' · '),
    voice_s: u.voice_s, grant_s: u.grant_s, calls: u.calls, encrypted: u.encrypted, max,
    title: 'Show this radio', onClick: () => setScope(unitScope(u.unit, u.alias)),
  });
  const rowsOf = (items, row) => {
    const max = Math.max(0, ...items.map(x => x.voice_s + x.grant_s));
    return items.map(x => row(x, max));
  };

  // The list: the busiest, narrowed by what is typed; a number not listed is offered by itself.
  function renderWho() {
    const [, many, one] = WHO.find(w => w[0] === who);
    const id = x => (who === 'tg' ? x.tg : x.unit);
    const q = whoSearch.value.trim().toLowerCase();
    const items = whoItems.filter(x => !q || String(id(x)).includes(q) || (x.alias || '').toLowerCase().includes(q));
    const rows = rowsOf(items, who === 'tg' ? tgRow : unitRow);
    const typed = /^\d+$/.test(q) ? Number(q) : null;
    if (typed && !whoItems.some(x => id(x) === typed)) {
      rows.unshift(h('button', {
        class: 'who-row', type: 'button', title: `Not among the ${num(whoItems.length)} busiest: show it by its number`,
        onclick: () => setScope(who === 'tg' ? tgScope(typed, null) : unitScope(typed, null)),
      }, h('span', { class: 'who-name', text: `${one} ${typed}` }), h('span', { class: 'who-time', text: 'show' })));
    }
    whoBody.replaceChildren(...(rows.length ? rows : [h('div', { class: 'calls-empty', text: whoItems.length ? `No ${many.toLowerCase()} match.` : `No ${many.toLowerCase()} in this period.` })]));
  }

  // What is known of the talkgroup or radio the page is narrowed to.
  function renderScope(d) {
    const body = [];
    if (d.radio) {
      const r = d.radio;
      setText(scopeKicker, `Radio ${r.unit}`);
      setText(scopeTitle, r.alias || String(r.unit));
      body.push(h('h3', { class: 'who-part', text: 'Talkgroups used' }),
        ...(r.talkgroups.length ? rowsOf(r.talkgroups, tgRow) : [h('div', { class: 'calls-empty', text: 'No calls in this period.' })]));
      const kinds = { group_affiliation: 'Affiliated', registration: 'Registered', deregistration: 'Deregistered' };
      body.push(h('h3', { class: 'who-part', text: 'Affiliations and registrations (all time)' }), r.events.length
        ? h('div', { class: 'act-pad' }, table(['Event', 'Talkgroup', 'Count', 'Last'], r.events.map(e => h('tr', { title: `First ${when(e.first_ms)}` },
          h('td', { text: kinds[e.kind] || e.kind }), h('td', { text: e.tg ? tgLabel(e.tg, e.alias) : DASH }),
          h('td', { text: num(e.count) }), h('td', { text: when(e.last_ms) })))))
        : h('div', { class: 'calls-empty', text: 'None seen.' }));
    } else {
      const t = d.talkgroup;
      setText(scopeKicker, `Talkgroup ${t.tg}`);
      setText(scopeTitle, tgLabel(t.tg, t.alias));
      body.push(h('dl', { class: 'kv act-pad' },
        h('dt', { text: 'Calls' }), h('dd', { text: num(t.calls) + (t.encrypted ? ', ' + num(t.encrypted) + ' encrypted' : '') }),
        h('dt', { text: 'Last encrypted' }), h('dd', { text: when(t.last_encrypted_ms) }),
        h('dt', { text: 'First encrypted' }), h('dd', { text: when(t.first_encrypted_ms) }),
        h('dt', { text: 'Last clear' }), h('dd', { text: when(t.last_clear_ms) }),
        h('dt', { text: 'Radios affiliated' }), h('dd', { text: num(t.affiliated_radios) + ' (all time)' })));
      body.push(h('h3', { class: 'who-part', text: 'Radios' }),
        ...(t.radios.length ? rowsOf(t.radios, unitRow) : [h('div', { class: 'calls-empty', text: 'No calls in this period.' })]));
    }
    whoBody.replaceChildren(...body);
  }

  // `total`: the period's calls as the history's hourly totals count them (from the start of its
  // first hour), shown beside the tabs; the list's own end is known only by the rows read.
  function renderCalls(items, total) {
    // The picked call takes what the list now knows of it.
    const now = picked && items.find(x => callKey(x) === picked.key);
    if (now) picked.x = now;
    const c = ctx();
    keyedList(callList, items, callKey, () => createRow(pickCall), (el, x) => updateRow(el, x, c));
    callsEmpty.hidden = items.length > 0;
    setText(callsEmpty, 'No calls in this period.');
    const more = moreRow(items.length, null, st.callRows, 'calls', rows => { st.callRows = rows; load(); });
    callsMore.replaceChildren(...(more ? [more] : []));
    setText(listCount, plural(total, 'call', 'calls'));
  }

  function renderAlerts(d) {
    alertGroups.hidden = !d.groups.length;
    alertGroups.replaceChildren(...(d.groups.length ? [table(['Alert', 'Tones', 'Times', 'Sent by', 'Talkgroups', 'First', 'Last'], d.groups.map(g => h('tr', null,
      h('td', null, h('span', { class: 'badge alert', text: kindLabel(g.kind) })),
      h('td', { text: tonesLabel(g.tones_hz) }),
      h('td', { text: num(g.count) }),
      h('td', { text: g.sources.join(', ') || DASH }),
      h('td', { text: g.talkgroups.join(', ') }),
      h('td', { text: when(g.first_ms) }),
      h('td', { text: when(g.last_ms) }))))] : []));
    keyedList(alertList, d.items, alertKey, a => alertRow(a, pickAlert), (el, a) => setClass(el, 'sel', !!picked && picked.alert === alertKey(a)));
    const total = d.groups.reduce((n, g) => n + g.count, 0);
    const more = d.items.length
      ? moreRow(d.items.length, total, st.alertRows, 'alerts', rows => { st.alertRows = rows; load(); })
      : h('div', { class: 'calls-empty', text: 'No alert tones in this period.' });
    alertsMore.replaceChildren(...(more ? [more] : []));
    setText(listCount, plural(total, 'alert', 'alerts'));
  }

  function renderControls() {
    chips.forEach(([p, b]) => setClass(b, 'on', !st.range && p === st.period));
    setClass(datesChip, 'on', !!st.range);
    dates.hidden = !st.range;
    if (st.range) {
      fromIn.value = hourValue(st.range.from);
      toIn.value = hourValue(st.range.to);
    }
    csv.href = '/api/v1/activity/calls?' + query({ format: 'csv', ...scopeQuery() });
    whoChips.forEach((chip, i) => {
      setClass(chip, 'on', WHO[i][0] === who);
      chip.setAttribute('aria-pressed', String(WHO[i][0] === who));
    });
    whoListHead.hidden = !!st.scope;
    whoScopeHead.hidden = !st.scope;
    if (st.scope) {
      setText(scopeKicker, st.scope.kind === 'tg' ? `Talkgroup ${st.scope.id}` : `Radio ${st.scope.id}`);
      setText(scopeTitle, st.scope.label);
    }
    tabChips.forEach((chip, i) => {
      setClass(chip, 'on', TABS[i][0] === tab);
      chip.setAttribute('aria-pressed', String(TABS[i][0] === tab));
    });
    callsBox.hidden = tab !== 'calls';
    alertsBox.hidden = tab !== 'alerts';
    dataBox.hidden = tab !== 'data';
    alertNote.btn.hidden = tab !== 'alerts';
  }

  let seq = 0;
  let loadedAt = 0;
  // The automatic reload that is waiting, if any: a load asked for sooner takes its place.
  let later = null;
  // Only what is on screen is read: the list or the one narrowed to, and the picked tab's rows.
  async function load() {
    const my = ++seq;
    loadedAt = Date.now();
    clearTimeout(later);
    later = null;
    renderControls();
    try {
      const sites = await api.activity('sites');
      if (my !== seq) return;
      renderSites(sites);
      renderControls();
      // No site to show (none live, and no history yet): the page is empty, with nothing asked.
      if (!st.site) {
        renderSummary(NOTHING, []);
        plot.update([], 3600000);
        whoItems = [];
        renderWho();
        if (tab === 'calls') renderCalls([], 0);
        else if (tab === 'alerts') renderAlerts({ groups: [], items: [] });
        else packets.update({});
        return;
      }
      const f = scopeQuery();
      const scope = st.scope;
      const site = st.site && !st.site.startsWith('system:') ? st.site : null;
      const [sum, series, people, rows] = await Promise.all([
        api.activity('summary', query()),
        api.activity('series', query({ ...buckets(), ...f })),
        scope
          ? api.activity((scope.kind === 'tg' ? 'talkgroup/' : 'radio/') + scope.id, query())
          : api.activity(who === 'tg' ? 'talkgroups' : 'radios', query({ limit: WHO_ROWS })),
        tab === 'calls' ? api.activity('calls', query({ limit: st.callRows, ...f }))
          : tab === 'alerts' ? api.activity('alerts', query({ limit: st.alertRows, ...f }))
            : api.data(site ? new URLSearchParams({ site }).toString() : ''),
      ]);
      if (my !== seq) return;
      const totals = renderSummary(sum.summary, series.buckets);
      plot.update(series.buckets, series.bucket_ms);
      if (scope) {
        renderScope(people);
      } else {
        whoItems = people.items;
        renderWho();
      }
      if (tab === 'calls') {
        renderCalls(rows.items, totals.calls);
      } else if (tab === 'alerts') {
        renderAlerts(rows);
      } else {
        packets.update(rows);
        setText(listCount, '');
      }
      showPicked(false);
    } catch (e) {
      if (my === seq) toast('Activity: ' + e.message, true);
    }
  }

  load();
  // The history changes as calls close (the store has them pushed): reload then, at most as
  // often as the period's `refresh`; a close inside it reloads when it is over. The picked call
  // plays on in its panel while the lists move. Picked dates are not reloaded.
  let seenCalls = null;
  return {
    update(store) {
      s = store;
      if (!s.calls || s.calls.recent === seenCalls) return;
      const first = seenCalls === null;
      seenCalls = s.calls.recent;
      if (first || later || st.range) return;
      later = setTimeout(() => {
        later = null;
        if (!st.range) load();
      }, Math.max(0, loadedAt + st.period.refresh - Date.now()));
    },
    unmount() {
      gone = true;
      // A load still under way draws and reports nothing.
      seq += 1;
      clearTimeout(later);
      plot.destroy();
      detail.stop();
    },
  };
}
