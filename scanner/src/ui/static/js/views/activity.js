// Activity: the per-site history: who talks, on which talkgroups, how much, what is encrypted,
// per hour or per day, and the alert tones heard (consoles' warbles and beeps, two-tone pages).
// It shows the last 24 hours, 7 or 30 days, or the hours between two dates.
//
// Two kinds of time are shown apart. Voice is decoded on the voice channel (followed calls) and
// is measured. Grant time is all that is known of a call that was not followed (encrypted, or no
// lane free): from the grant to its last update on the control channel. It includes hang time and
// any other radio keying up on the grant, and is credited to the radio granted.

import { h, setText, card, toast, table, icon, keyedList } from '../dom.js';
import { api } from '../api.js';
import { recordingPlayer } from '../audio/recording.js';
import { packetDataCard } from './packet_data.js';
import { dur, num, bytes, tgLabel, unitLabel, DASH, dayTime as when } from '../format.js';

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
// How often a reload held back by a playing recording looks again.
const PLAYING_CHECK_MS = 2000;
const ALERT_NOTE = 'Found in the decoded audio of followed calls. The vocoder rebuilds a tone to within about 1 % of what '
  + 'was sent, so the same tones read a little differently from call to call. The dispatch is what the alert announced: '
  + 'the next transmission of the same radio on the talkgroup with 4 s of voice, each of its key-ups within 10 s of its last '
  + '(another radio\'s answer between is passed over); without one, the longest of them with 3 s.';
const GRANT_NOTE = 'Encrypted and not-followed calls show grant time: from the grant to its last update on the control channel. '
  + 'It includes hang time and any other radio that keyed up on the grant, and is credited to the radio granted. '
  + 'Voice is decoded on the voice channel.';


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


function clickRow(cells, onClick, title) {
  const tr = h('tr', { style: { cursor: 'pointer' }, title: title || '' }, ...cells.map(c => (c instanceof Node ? h('td', null, c) : h('td', { text: c }))));
  tr.addEventListener('click', onClick);
  return tr;
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

// A call's recording, played from `offsetMs`.
function playCall(callId, offsetMs, text, title) {
  const btn = h('button', { class: 'btn small', type: 'button', title }, icon('play'), text);
  btn.addEventListener('click', () => {
    const rec = recordingPlayer(`/api/v1/recordings/${callId}.wav`);
    btn.replaceWith(rec.el);
    rec.play(offsetMs / 1000);
  });
  return btn;
}

// What an alert announced: its dispatch call (the sending radio's next transmission with 4 s of
// voice), else the tone itself.
function playAlert(a) {
  const d = a.dispatch;
  if (d && d.recorded) {
    const where = d.call_id === a.call_id ? 'after the tone in the same call' : `call #${d.call_id}`;
    return playCall(d.call_id, d.offset_ms, 'Play', `Play the dispatch: ${where}, ${dur(d.voice_ms)}`);
  }
  if (a.recorded) return playCall(a.call_id, a.offset_ms, 'Tone', `No dispatch found: play the alert tone (call #${a.call_id})`);
  return h('span', { class: 'faint', text: 'not recorded' });
}

function encBadge(n) {
  return n ? h('span', { class: 'badge enc', text: String(n) }) : '0';
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

// An alert's row: the same alert with the same dispatch keeps its row, and the recording it plays.
function alertKey(a) {
  const d = a.dispatch;
  return [a.site, a.call_id, a.at_ms, a.recorded ? 1 : 0, d ? `${d.call_id}:${d.recorded ? 1 : 0}` : ''].join('|');
}

function alertRow(a) {
  return h('tr', null,
    h('td', { text: stamp(a.at_ms) }),
    h('td', { text: tgLabel(a.tg, a.tg_name) }),
    h('td', { text: unitLabel(a.source, a.source_name) }),
    h('td', null, h('span', { class: 'badge alert', text: kindLabel(a.kind) })),
    h('td', { text: tonesLabel(a.tones_hz) }),
    h('td', { text: a.dispatch ? dur(a.dispatch.voice_ms) : DASH, title: a.dispatch ? `Call #${a.dispatch.call_id}` : 'No transmission of 3 s from the same radio followed, each key-up within 10 s' }),
    h('td', null, playAlert(a)));
}

// Under a list of the newest rows: how many it shows, and a button for more. `total` is null
// when only the rows read are known; nothing is shown under a list that has them all.
function moreRow(shown, total, rows, what, onMore) {
  if (total === null ? shown < rows : shown >= total) return null;
  const count = total === null ? `The newest ${num(shown)} ${what}.` : `The newest ${num(shown)} of ${num(total)} ${what}.`;
  if (rows >= LIST_MAX) return h('p', { class: 'card-note', text: `${count} Pick dates to see the others.` });
  const btn = h('button', { class: 'btn small', type: 'button', text: 'Show more' });
  btn.addEventListener('click', () => onMore(Math.min(LIST_MAX, rows * 2)));
  return h('div', { class: 'row', style: { marginTop: '10px' } }, h('span', { class: 'dim', text: count }), btn);
}

// Stacked bars: voice, clear grant-only, encrypted grant-only, per bucket.
function chart() {
  const canvas = h('canvas', { style: { width: '100%', height: '220px', display: 'block' }, 'aria-label': 'Airtime per bucket' });
  const readout = h('p', { class: 'card-note', text: ' ' });
  const legend = h('div', { class: 'row', style: { gap: '14px', fontSize: '0.84em', marginTop: '8px' } });
  let data = null;
  function key(color, text) {
    return h('span', { class: 'row', style: { gap: '6px' } },
      h('span', { style: { width: '12px', height: '12px', borderRadius: '2px', background: color, display: 'inline-block' } }),
      h('span', { text }));
  }
  function draw() {
    if (!data) return;
    const { buckets, bucketMs } = data;
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth || 600;
    const hgt = canvas.clientHeight || 220;
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
    // X labels: about six, at bucket starts.
    g.fillStyle = colors.text;
    g.textAlign = 'center';
    g.textBaseline = 'top';
    const every = Math.max(1, Math.ceil(n / 6));
    for (let i = 0; i < n; i += every) {
      const d = new Date(buckets[i].t);
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
    const d = new Date(b.t);
    const at = data.bucketMs >= 86400000 ? d.toLocaleDateString() : d.toLocaleDateString() + ' ' + String(d.getHours()).padStart(2, '0') + ':00';
    setText(readout, at + ': ' + b.calls + ' call' + (b.calls === 1 ? '' : 's') + (b.encrypted ? ' (' + b.encrypted + ' encrypted)' : '')
      + '; voice ' + secs(b.voice_s) + ', clear grant ' + secs(b.clear_grant_s) + ', encrypted grant ' + secs(b.encrypted_grant_s) + '.');
  });
  canvas.addEventListener('mouseleave', () => setText(readout, ' '));
  window.addEventListener('resize', draw);
  return {
    el: h('div', null, canvas, legend, readout),
    update(buckets, bucketMs) { data = { buckets, bucketMs }; draw(); },
    destroy() { window.removeEventListener('resize', draw); },
  };
}

export function mount(host) {
  // `range` ({ from, to }, unix ms on whole hours) is the picked dates; without it the page shows
  // the last `period`. `alertRows` and `callRows` are how many rows the two lists ask for.
  const st = { period: PERIODS[0], range: null, site: null, filter: null, detail: null, alertRows: LIST_ROWS, callRows: LIST_ROWS };
  const tz = -new Date().getTimezoneOffset();
  const firstRows = () => { st.alertRows = LIST_ROWS; st.callRows = LIST_ROWS; };

  // Controls.
  const ctl = card('Activity');
  const siteSel = h('select', { 'aria-label': 'Site' });
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
  const dates = h('div', { class: 'row', style: { gap: '6px' }, hidden: true },
    h('span', { class: 'dim', text: 'From' }), fromIn, h('span', { class: 'dim', text: 'to' }), toIn);
  const csv = h('a', { class: 'btn small', text: 'Export CSV', href: '#' });
  const filterNote = h('span', { class: 'row', style: { gap: '6px' } });
  const store = h('p', { class: 'card-note' });
  ctl.body.append(
    h('div', { class: 'row', style: { gap: '10px', flexWrap: 'wrap' } },
      h('label', { class: 'row', style: { gap: '6px' } }, h('span', { text: 'Site' }), siteSel),
      h('div', { class: 'row', style: { gap: '6px' } }, ...chips.map(c => c[1]), datesChip),
      dates, csv, filterNote),
    store);
  siteSel.addEventListener('change', () => { st.site = siteSel.value; st.filter = null; st.detail = null; firstRows(); load(); });
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

  // Summary + chart.
  const sum = card('Summary');
  const m = {
    calls: metric('Calls', DASH),
    followed: metric('Followed', DASH),
    enc: metric('Encrypted', DASH),
    voice: metric('Voice', DASH, 'Decoded on the voice channel'),
    clear: metric('Clear, not followed', DASH, 'Grant time of clear calls no lane followed'),
    encTime: metric('Encrypted grant', DASH, 'Grant time of encrypted calls (control channel)'),
    est: metric('Encrypted, est. voice', DASH, 'Encrypted grant time x the decoded voice per grant second of this window\'s followed calls'),
    tgs: metric('Talkgroups', DASH),
    radios: metric('Radios', DASH),
  };
  const plot = chart();
  sum.body.append(
    h('div', { style: { display: 'grid', gap: '10px 18px', gridTemplateColumns: 'repeat(auto-fill, minmax(120px, 1fr))' } }, ...Object.values(m).map(x => x.el)),
    h('div', { style: { marginTop: '14px' } }, plot.el),
    h('p', { class: 'card-note', text: GRANT_NOTE }));

  // The alerts: their groups, then the newest. A row stays while its alert is listed, so a
  // recording played from it plays on through a reload.
  const alertCard = card('Alert tones');
  const alertGroups = h('div');
  const newest = table(['Time', 'Talkgroup', 'From', 'Alert', 'Tones', 'Dispatch', ''], []);
  const newestRows = newest.querySelector('tbody');
  const newestBox = h('div', { hidden: true }, h('h3', { text: 'Newest' }), newest);
  const alertMore = h('div');
  alertCard.body.append(alertGroups, newestBox, alertMore, h('p', { class: 'card-note', text: ALERT_NOTE }));
  const tgCard = card('Talkgroups');
  const radioCard = card('Radios');
  const detailCard = card('Details');
  detailCard.el.hidden = true;
  const callsCard = card('Recent calls');
  const packets = packetDataCard();

  host.append(h('div', { class: 'stack' }, ctl.el, sum.el, alertCard.el, h('div', { class: 'grid-2' }, tgCard.el, radioCard.el), detailCard.el, callsCard.el, packets.el));

  // `st.site` is a site id, or `system:<id>` for every site of a system.
  function query(extra = {}) {
    const q = new URLSearchParams(st.range ? { from: String(st.range.from), to: String(st.range.to) } : { hours: String(st.period.hours) });
    if (st.site && st.site.startsWith('system:')) q.set('system', st.site.slice(7));
    else if (st.site) q.set('site', st.site);
    for (const [k, v] of Object.entries(extra)) if (v !== undefined && v !== null) q.set(k, String(v));
    return q.toString();
  }
  function filterQuery() {
    if (!st.filter) return {};
    return st.filter.kind === 'tg' ? { tg: st.filter.id } : { unit: st.filter.id };
  }
  function setFilter(f) {
    st.filter = f;
    st.detail = f;
    firstRows();
    load();
  }
  // The chart's buckets: the period's, or by the length of the picked dates, with days starting
  // at the midnight of the dates' first day.
  function buckets() {
    if (!st.range) return { bucket: st.period.bucket, tz };
    return { bucket: st.range.to - st.range.from <= HOURLY_MAX ? 'hour' : 'day', tz: -new Date(st.range.from).getTimezoneOffset() };
  }

  function renderSites(d) {
    const sites = d.sites.slice();
    if (d.active && !sites.some(s => s.site === d.active)) sites.unshift({ site: d.active, label: d.active, calls: 0 });
    if (!st.site) st.site = d.active || (sites[0] && sites[0].site) || null;
    // A system with several sites with history can be shown as a whole.
    const systems = new Map();
    for (const s of sites) if (s.system) systems.set(s.system, (systems.get(s.system) || 0) + 1);
    const whole = [...systems].filter(([, n]) => n > 1).map(([id]) =>
      h('option', { value: 'system:' + id, text: 'All sites of ' + id }));
    siteSel.replaceChildren(...sites.map(s => h('option', { value: s.site, text: (s.label || s.site) + (s.site === d.active ? ' (active)' : '') + ' — ' + num(s.calls) + ' calls' })), ...whole);
    siteSel.value = st.site;
    setText(store, 'Kept ' + d.retention_days + ' days, up to ' + bytes(d.max_bytes) + ' (' + bytes(d.used_bytes) + ' used), in ' + d.database
      + (d.on_sd ? ' (SD card).' : ' (RAM: lost on reboot; insert an SD card to keep it).'));
  }

  function renderSummary(s) {
    setText(m.calls.v, num(s.calls));
    setText(m.followed.v, num(s.followed));
    setText(m.enc.v, num(s.encrypted));
    setText(m.voice.v, secs(s.voice_s));
    setText(m.clear.v, secs(s.clear_grant_s));
    setText(m.encTime.v, secs(s.encrypted_grant_s));
    setText(m.est.v, s.voice_per_grant && s.encrypted_grant_s
      ? '≈ ' + secs(s.encrypted_grant_s * s.voice_per_grant) + ' (×' + s.voice_per_grant.toFixed(2) + ')'
      : DASH);
    setText(m.tgs.v, num(s.talkgroups));
    setText(m.radios.v, num(s.radios));
  }

  function renderTgs(items) {
    tgCard.body.replaceChildren(items.length
      ? table(['Talkgroup', 'Calls', 'Enc', 'Voice', 'Grant', 'Radios', 'Last'], items.map(t => clickRow(
        [tgLabel(t.tg, t.alias), num(t.calls), encBadge(t.encrypted), secs(t.voice_s), secs(t.grant_s), num(t.radios), when(t.last_ms)],
        () => setFilter({ kind: 'tg', id: t.tg, label: tgLabel(t.tg, t.alias) }), 'Show this talkgroup')))
      : h('p', { class: 'card-note', text: 'No calls in this period.' }));
  }

  function renderRadios(items) {
    radioCard.body.replaceChildren(items.length
      ? table(['Radio', 'Calls', 'Enc', 'Voice', 'Grant', 'TGs', 'Last'], items.map(u => clickRow(
        [unitLabel(u.unit, u.alias), num(u.calls), encBadge(u.encrypted), secs(u.voice_s), secs(u.grant_s), num(u.talkgroups), when(u.last_ms)],
        () => setFilter({ kind: 'unit', id: u.unit, label: unitLabel(u.unit, u.alias) }), 'Show this radio')))
      : h('p', { class: 'card-note', text: 'No radios in this period.' }));
  }

  function renderDetail(d) {
    detailCard.el.hidden = !d;
    if (!d) return;
    const body = [];
    if (d.radio) {
      const r = d.radio;
      setText(detailCard.head.querySelector('h2'), 'Radio ' + unitLabel(r.unit, r.alias));
      body.push(h('h3', { text: 'Talkgroups used' }), r.talkgroups.length
        ? table(['Talkgroup', 'Calls', 'Enc', 'Voice', 'Grant', 'Last'], r.talkgroups.map(t => clickRow(
          [tgLabel(t.tg, t.alias), num(t.calls), encBadge(t.encrypted), secs(t.voice_s), secs(t.grant_s), when(t.last_ms)],
          () => setFilter({ kind: 'tg', id: t.tg, label: tgLabel(t.tg, t.alias) }), 'Show this talkgroup')))
        : h('p', { class: 'card-note', text: 'No calls in this period.' }));
      const kinds = { group_affiliation: 'Affiliated', registration: 'Registered', deregistration: 'Deregistered' };
      body.push(h('h3', { text: 'Affiliations and registrations (all time)' }), r.events.length
        ? table(['Event', 'Talkgroup', 'Count', 'First', 'Last'], r.events.map(e => h('tr', null,
          h('td', { text: kinds[e.kind] || e.kind }), h('td', { text: e.tg ? tgLabel(e.tg, e.alias) : DASH }),
          h('td', { text: num(e.count) }), h('td', { text: when(e.first_ms) }), h('td', { text: when(e.last_ms) }))))
        : h('p', { class: 'card-note', text: 'None seen.' }));
    } else if (d.talkgroup) {
      const t = d.talkgroup;
      setText(detailCard.head.querySelector('h2'), tgLabel(t.tg, t.alias));
      body.push(h('dl', { class: 'kv' },
        h('dt', { text: 'Calls' }), h('dd', { text: num(t.calls) + (t.encrypted ? ', ' + num(t.encrypted) + ' encrypted' : '') }),
        h('dt', { text: 'Last encrypted' }), h('dd', { text: when(t.last_encrypted_ms) }),
        h('dt', { text: 'First encrypted' }), h('dd', { text: when(t.first_encrypted_ms) }),
        h('dt', { text: 'Last clear' }), h('dd', { text: when(t.last_clear_ms) }),
        h('dt', { text: 'Radios affiliated' }), h('dd', { text: num(t.affiliated_radios) + ' (all time)' })));
      body.push(h('h3', { text: 'Radios' }), t.radios.length
        ? table(['Radio', 'Calls', 'Enc', 'Voice', 'Grant', 'Last'], t.radios.map(u => clickRow(
          [unitLabel(u.unit, u.alias), num(u.calls), encBadge(u.encrypted), secs(u.voice_s), secs(u.grant_s), when(u.last_ms)],
          () => setFilter({ kind: 'unit', id: u.unit, label: unitLabel(u.unit, u.alias) }), 'Show this radio')))
        : h('p', { class: 'card-note', text: 'No calls in this period.' }));
    }
    detailCard.body.replaceChildren(...body);
  }

  function renderAlerts(d) {
    alertGroups.replaceChildren(d.groups.length
      ? table(['Alert', 'Tones', 'Times', 'Sent by', 'Talkgroups', 'First', 'Last'], d.groups.map(g => h('tr', null,
        h('td', null, h('span', { class: 'badge alert', text: kindLabel(g.kind) })),
        h('td', { text: tonesLabel(g.tones_hz) }),
        h('td', { text: num(g.count) }),
        h('td', { text: g.sources.join(', ') || DASH }),
        h('td', { text: g.talkgroups.join(', ') }),
        h('td', { text: when(g.first_ms) }),
        h('td', { text: when(g.last_ms) }))))
      : h('p', { class: 'card-note', text: 'No alert tones in this period.' }));
    keyedList(newestRows, d.items, alertKey, alertRow, () => {});
    newestBox.hidden = !d.items.length;
    const total = d.groups.reduce((n, g) => n + g.count, 0);
    const more = moreRow(d.items.length, total, st.alertRows, 'alerts', rows => { st.alertRows = rows; load(); });
    alertMore.replaceChildren(...(more ? [more] : []));
  }

  function renderCalls(items) {
    const more = moreRow(items.length, null, st.callRows, 'calls', rows => { st.callRows = rows; load(); });
    callsCard.body.replaceChildren(items.length
      ? table(['Start', 'Talkgroup', 'Radios', 'Voice', 'Grant', ''], items.map(c => h('tr', null,
        h('td', { text: when(c.started_unix_ms) }),
        h('td', { text: 'TG ' + c.tg + (c.tg_name ? ` ${c.tg_name}` : '') }),
        h('td', { text: c.sources.length ? c.sources.join(', ') : DASH }),
        h('td', { text: c.voice_frames ? dur(c.voice_frames * 20) : DASH }),
        h('td', { text: dur(c.grant_ms) }),
        h('td', null, c.encrypted ? h('span', { class: 'badge enc', text: 'enc' })
          : (c.not_followed ? h('span', { class: 'badge hang', text: c.not_followed }) : '')))))
      : h('p', { class: 'card-note', text: 'No calls in this period.' }), ...(more ? [more] : []));
  }

  function renderFilter() {
    chips.forEach(([p, b]) => b.classList.toggle('on', !st.range && p === st.period));
    datesChip.classList.toggle('on', !!st.range);
    dates.hidden = !st.range;
    if (st.range) {
      fromIn.value = hourValue(st.range.from);
      toIn.value = hourValue(st.range.to);
    }
    filterNote.replaceChildren();
    if (st.filter) {
      const clear = h('button', { type: 'button', class: 'btn small', text: 'Show all' });
      clear.addEventListener('click', () => { st.filter = null; st.detail = null; load(); });
      filterNote.append(h('span', { class: 'badge ok', text: st.filter.label }), clear);
    }
    csv.href = '/api/v1/activity/calls?' + query({ format: 'csv', ...filterQuery() });
    setText(callsCard.head.querySelector('h2'), st.filter ? 'Recent calls: ' + st.filter.label : 'Recent calls');
  }

  let seq = 0;
  let loadedAt = 0;
  async function load() {
    const my = ++seq;
    loadedAt = Date.now();
    renderFilter();
    try {
      const sites = await api.activity('sites');
      if (my !== seq) return;
      renderSites(sites);
      renderFilter();
      const f = filterQuery();
      const detail = st.detail
        ? api.activity((st.detail.kind === 'tg' ? 'talkgroup/' : 'radio/') + st.detail.id, query())
        : Promise.resolve(null);
      const site = st.site && !st.site.startsWith('system:') ? st.site : null;
      const [s, series, tgs, radios, calls, det, data, alerts] = await Promise.all([
        api.activity('summary', query()),
        api.activity('series', query({ ...buckets(), ...f })),
        api.activity('talkgroups', query({ limit: 25 })),
        api.activity('radios', query({ limit: 25 })),
        api.activity('calls', query({ limit: st.callRows, ...f })),
        detail,
        api.data(site ? new URLSearchParams({ site }).toString() : ''),
        api.activity('alerts', query({ limit: st.alertRows, ...f })),
      ]);
      if (my !== seq) return;
      renderSummary(s.summary);
      plot.update(series.buckets, series.bucket_ms);
      renderTgs(tgs.items);
      renderRadios(radios.items);
      renderDetail(det);
      renderCalls(calls.items);
      renderAlerts(alerts);
      packets.update(data);
    } catch (e) {
      if (my === seq) toast('Activity: ' + e.message, true);
    }
  }

  load();
  // The history changes as calls close (the store has them pushed): reload then, at most as
  // often as the period's `refresh`; a close inside it reloads when it is over. A reload waits
  // while a recording plays, so the list does not move under it. Picked dates are not reloaded.
  const playing = () => [...newestRows.querySelectorAll('audio')].some(a => !a.paused);
  let seenCalls = null;
  let later = null;
  const reload = () => {
    if (playing()) {
      later = setTimeout(reload, PLAYING_CHECK_MS);
      return;
    }
    later = null;
    if (!st.range) load();
  };
  return {
    update(s) {
      if (!s.calls || s.calls.recent === seenCalls) return;
      const first = seenCalls === null;
      seenCalls = s.calls.recent;
      if (first || later || st.range) return;
      later = setTimeout(reload, Math.max(0, loadedAt + st.period.refresh - Date.now()));
    },
    unmount() {
      clearTimeout(later);
      plot.destroy();
    },
  };
}
