// Files (the rail's): what is on the card, to take away. The call recordings as one ZIP archive
// of a period, a site, a talkgroup, a radio or the locked ones, or of one day (this browser's)
// from the table;
// the history's database; each TV recording's file. A download is a link the browser saves as
// it arrives; the radio makes one at a time, and pushes how far it is while the page is open.

import { h, card, table, toast, setText, setClass, icon } from '../dom.js';
import { api } from '../api.js';
import { want } from '../store.js';
import { bytes, num } from '../format.js';

// The page asks what a selection takes this long after its last change.
const ASK_MS = 300;
// The listing is read again when recordings came in, at most this often.
const AGAIN_MS = 30000;
const ZIP = '/api/v1/files/recordings.zip';
const CARD_STATE = {
  unknown: 'not looked at yet', ok: 'in use', absent: 'not mounted', read_only: 'mounted read-only', full: 'full', error: 'failing', failing: 'the last write failed',
};

const plural = (n, one) => `${num(n)} ${one}${n === 1 ? '' : 's'}`;
const two = n => String(n).padStart(2, '0');
// A local date (`YYYY-MM-DD`, as a date field gives it) as its midnight, or the next day's.
const midnight = (date, next = 0) => {
  const [y, m, d] = date.split('-').map(Number);
  return new Date(y, m - 1, d + next).getTime();
};
// The minutes a span of the listing is long: the longest that every hour of this browser's zone
// is a whole number of (zones sit a multiple of 15 minutes off UTC).
const SPAN_MIN = [60, 30, 15].find(m => new Date().getTimezoneOffset() % m === 0);

// The listing's spans added up into this browser's days, the newest first: each with its date,
// its bounds (midnight to midnight here, whatever the clocks did that day) and its recordings.
function localDays(spans) {
  const days = new Map();
  for (const s of spans) {
    const at = new Date(s.from_ms);
    const day = `${at.getFullYear()}-${two(at.getMonth() + 1)}-${two(at.getDate())}`;
    let d = days.get(day);
    if (!d) {
      d = { day, from_ms: midnight(day), to_ms: midnight(day, 1), count: 0, bytes: 0, locked: 0 };
      days.set(day, d);
    }
    d.count += s.count;
    d.bytes += s.bytes;
    d.locked += s.locked;
  }
  return [...days.values()];
}

// A download's link. While one runs every link is off: the radio would refuse a second.
function link(text, title) {
  const a = h('a', { class: 'btn', title, download: '' }, icon('download', 16), h('span', { text }));
  a.addEventListener('click', e => {
    if (a.getAttribute('aria-disabled') === 'true') {
      e.preventDefault();
      toast(a.dataset.why || 'Not now', true);
    }
  });
  return a;
}

// `why`: the reason it is off; none: it is on.
function setLink(a, href, why) {
  if (href) a.setAttribute('href', href);
  else a.removeAttribute('href');
  a.setAttribute('aria-disabled', String(!!why || !href));
  a.dataset.why = why || '';
}

export function mount(el) {
  // The listing, the download running, and what the selection takes ({ count, bytes }).
  let listing = null;
  let download = null;
  let taken = null;
  let gone = false;
  let readAt = 0;
  let seenRecordings = null;
  const links = [];
  const busy = () => (download ? `${download.name} is being sent: one download at a time` : '');

  // ── The download running ──
  const runName = h('strong');
  const runText = h('span', { class: 'dim' });
  const runBar = h('progress', { class: 'scan', max: 1, value: 0 });
  const running = h('section', { class: 'card files-run', hidden: true, 'aria-live': 'polite' }, h('div', { class: 'row' }, runName, runText), runBar);

  // ── Recordings: the selection, then the days ──
  const rec = card('Recordings');
  const recTotals = h('p', { class: 'card-note files-totals' });
  const from = h('input', { class: 'input', type: 'date', 'aria-label': 'From' });
  const to = h('input', { class: 'input', type: 'date', 'aria-label': 'To' });
  const site = h('select', { class: 'input', 'aria-label': 'Site' }, h('option', { value: '', text: 'Every site' }));
  const tg = h('input', { class: 'input num', type: 'number', min: 0, step: 1, placeholder: 'any', 'aria-label': 'Talkgroup' });
  const unit = h('input', { class: 'input num', type: 'number', min: 0, step: 1, placeholder: 'any', 'aria-label': 'Radio' });
  const locked = h('input', { type: 'checkbox', 'aria-label': 'Only the locked ones' });
  const takes = h('p', { class: 'card-note', 'aria-live': 'polite' });
  const zip = link('Download ZIP', 'The selection as one ZIP archive, with a table of what it holds');
  links.push(zip);
  const field = (label, control) => h('label', { class: 'files-field' }, h('span', { class: 'dim', text: label }), control);
  const days = h('div', { class: 'files-days' });
  rec.body.append(
    recTotals,
    h('div', { class: 'files-pick' }, field('From', from), field('To', to), field('Site', site), field('Talkgroup', tg), field('Radio', unit),
      h('label', { class: 'chip' }, locked, h('span', { text: 'Only the locked ones' }))),
    h('div', { class: 'set-foot' }, takes, h('div', { class: 'set-ctl' }, zip)),
    h('p', { class: 'card-note', text: 'An archive holds each recording as it is on the card, in the folder of its call\'s date (UTC), and recordings.csv: the table of them with the talkgroups\' and radios\' names. It is not compressed.' }),
    days);

  // The selection as the routes take it; null when a field cannot be read.
  function query() {
    const q = new URLSearchParams();
    if (from.value) q.set('from', String(midnight(from.value)));
    if (to.value) q.set('to', String(midnight(to.value, 1)));
    if (site.value) q.set('site', site.value);
    for (const [name, input] of [['tg', tg], ['unit', unit]]) {
      const text = input.value.trim();
      if (!text) continue;
      if (!/^\d+$/.test(text)) return null;
      q.set(name, text);
    }
    if (locked.checked) q.set('locked', 'true');
    return q.toString();
  }

  let asking = 0;
  let timer = null;
  async function ask() {
    const q = query();
    const n = ++asking;
    if (q === null) {
      taken = null;
      setText(takes, 'A talkgroup and a radio are numbers.');
      drawLinks();
      return;
    }
    try {
      const t = await api.filesCount(q);
      if (gone || n !== asking) return;
      taken = { ...t, q };
      setText(takes, t.count ? `${plural(t.count, 'recording')}, ${bytes(t.bytes)}` : 'No recording is in the selection.');
    } catch (e) {
      if (gone || n !== asking) return;
      taken = null;
      setText(takes, e.message);
    }
    drawLinks();
  }
  for (const input of [from, to, site, tg, unit, locked]) {
    input.addEventListener('input', () => {
      clearTimeout(timer);
      timer = setTimeout(ask, ASK_MS);
    });
  }

  // ── The card, the history, TV ──
  const sd = card('SD card');
  const sdText = h('p', { class: 'files-line' });
  sd.body.append(sdText);
  const history = card('History');
  const historyText = h('p', { class: 'files-line' });
  const historyLink = link('Download', 'The history\'s database as one file');
  links.push(historyLink);
  history.body.append(
    h('div', { class: 'set-foot' }, historyText, h('div', { class: 'set-ctl' }, historyLink)),
    h('p', { class: 'card-note' }, 'Every call the scanner has seen, as its SQLite database. The radio makes a copy on the card first, so the download starts when that is done. A period\'s calls as a table are on ',
      h('a', { href: '#activity', text: 'Activity' }), ' (CSV).'));
  const tv = card('TV recordings');
  const tvRows = h('div');
  tv.body.append(tvRows, h('p', { class: 'card-note', text: 'A recorded channel\'s transport stream, as it is on the card.' }));

  el.append(h('div', { class: 'files' },
    h('p', { class: 'card-note files-where', text: 'What is on the card, to take away. The radio sends one download at a time, and goes on following calls meanwhile.' }),
    running, rec.el, h('div', { class: 'stack' }, sd.el, history.el, tv.el)));

  function drawLinks() {
    const why = busy();
    setLink(zip, taken && taken.count ? `${ZIP}${taken.q ? `?${taken.q}` : ''}` : null, why || (taken && !taken.count ? 'No recording is in the selection' : ''));
    setLink(historyLink, '/api/v1/files/history.sqlite', why);
    for (const a of links) if (a.dataset.href) setLink(a, a.dataset.href, why);
  }

  function drawRunning() {
    running.hidden = !download;
    if (!download) return;
    const d = download;
    setText(runName, d.name);
    setText(runText, d.copying ? 'the radio is making its copy…'
      : d.what === 'recordings' ? `${num(d.files_done)} of ${plural(d.files, 'recording')} · ${bytes(d.bytes_done)} of ${bytes(d.bytes)}${d.skipped ? ` · ${num(d.skipped)} left out` : ''}`
        : `${bytes(d.bytes_done)} of ${bytes(d.bytes)}`);
    if (d.copying) runBar.removeAttribute('value');
    else runBar.value = d.bytes ? Math.min(1, d.bytes_done / d.bytes) : 0;
  }

  function drawListing() {
    const l = listing;
    const r = l.recordings;
    setText(recTotals, `${plural(r.sd.count, 'recording')} on the card, ${bytes(r.sd.bytes)}; ${num(r.locked.count)} of them locked${r.ram.count ? `; ${num(r.ram.count)} in RAM` : ''}.`);
    // The rows' own links leave the list of links with the rows.
    links.length = 2;
    const row = d => {
      const a = link('ZIP', `The recordings of ${d.day} as one ZIP archive`);
      a.classList.add('small');
      a.dataset.href = `${ZIP}?from=${d.from_ms}&to=${d.to_ms}`;
      links.push(a);
      return h('tr', null, h('td', { text: d.day }), h('td', { text: num(d.count) }), h('td', { text: bytes(d.bytes) }), h('td', { text: d.locked ? num(d.locked) : '' }), h('td', { class: 'files-get' }, a));
    };
    days.replaceChildren(r.spans.length ? table(['Day', 'Recordings', 'Size', 'Locked', ''], localDays(r.spans).map(row)) : h('p', { class: 'dim', text: 'No recordings yet.' }));
    const c = l.card;
    const space = c.free_bytes !== null && c.total_bytes !== null ? `: ${bytes(c.free_bytes)} free of ${bytes(c.total_bytes)}` : '';
    setText(sdText, `${CARD_STATE[c.state] || c.state}${space}${c.detail ? ` (${c.detail})` : ''}`);
    setText(historyText, `${l.history.name} · ${bytes(l.history.bytes)} · ${l.history.on_sd ? 'on the card' : 'in RAM: no card'}`);
    tvRows.replaceChildren(...(l.tv.length ? l.tv.map(t => {
      const a = link('Download', `RF ${t.rf}'s recording as a file`);
      a.dataset.href = `/api/v1/files/tv/rf${t.rf}.ts`;
      links.push(a);
      return h('div', { class: 'set-row' },
        h('div', { class: 'set-what' }, h('div', { text: `RF ${t.rf}` }), h('div', { class: 'sub', text: `rf${t.rf}.ts · ${bytes(t.bytes)}${t.programs.length ? ` · ${plural(t.programs.length, 'programme')}` : ''}` })),
        h('div', { class: 'set-ctl' }, a));
    }) : [h('p', { class: 'dim', text: 'None on the card.' })]));
    drawLinks();
  }

  async function read() {
    readAt = Date.now();
    try {
      const l = await api.files(SPAN_MIN);
      if (gone) return;
      listing = l;
      download = l.download;
      drawListing();
      drawRunning();
    } catch (e) {
      if (!gone) toast(e.message, true);
    }
  }

  // How far the download is, as the radio pushes it; one that ended changed nothing on the card,
  // but what else was recorded meanwhile is read with it.
  const stop = want({ files: true }, m => {
    const ended = !!download && !m.download;
    download = m.download;
    drawRunning();
    drawLinks();
    if (ended) read();
  });
  setLink(zip, null, '');
  read();
  ask();

  // The sites the store knows, once.
  let sitesFrom = null;
  return {
    update(s) {
      if (s.systems && s.systems !== sitesFrom) {
        sitesFrom = s.systems;
        const picked = site.value;
        site.replaceChildren(h('option', { value: '', text: 'Every site' }),
          ...s.systems.flatMap(sys => sys.sites.map(x => h('option', { value: x.id, text: `${sys.label} · ${x.label}` }))));
        site.value = [...site.options].some(o => o.value === picked) ? picked : '';
      }
      // Recordings came in or went: the listing again, not at every one.
      if (s.recordings !== seenRecordings) {
        const first = seenRecordings === null;
        seenRecordings = s.recordings;
        if (!first && Date.now() - readAt >= AGAIN_MS) {
          read();
          ask();
        }
      }
    },
    unmount() {
      gone = true;
      clearTimeout(timer);
      stop();
    },
  };
}
