// Now: the live site at a glance. Fixed at the top: the system card (the system and the site to
// listen to, picked from two lists; the site's details, identity and control channel health) and
// the traffic channels side by side, lane 1 on the left speaker and lane 2 on the right. Below
// them the live site's calls scroll in their own pane, newest first, each with its recording.
// Everything comes from the store, which the radio pushes.

import { h, card, toast, setText, setClass, keyedList, icon } from '../dom.js';
import { api } from '../api.js';
import { refresh } from '../store.js';
import { mhz, ago, pct, num, dur, clock, bytes, unitLabel, DASH, NOT_FOLLOWED } from '../format.js';
import { protocol } from '../protocols.js';

// A control channel with no message for this long is shown as silent.
const SILENT_MS = 5000;
// Voice this recent: the call is talking; after it, the call hangs until it closes.
const VOICE_HOLD_MS = 1500;
// The traffic cards' clocks move between pushes, and so do the call list's ages.
const TICK_MS = 250;
const AGES_MS = 5000;
// The follower serves the left speaker's talkgroups on lane 1 and the right's on lane 2.
const SIDE = { 1: 'Left', 2: 'Right' };
const PHASE = { acquiring: 'Acquiring', voice: 'Voice', hang: 'Hang', ending: 'Ended' };

// Kept while the page is left for another.
let showNotFollowed = true;

const boardNow = s => s.status.now_unix_ms + (Date.now() - s.statusAt);
const tgName = x => x.tg_name || `TG ${x.tg}`;
const known = v => v !== null && v !== undefined;

// Hold the live site on `tg`, or release the hold (null).
async function setHold(tg) {
  try {
    await api.setHold(tg);
    toast(tg === null ? 'Hold released' : `Holding TG ${tg}`);
    await refresh();
  } catch (e) {
    toast(e.message, true);
  }
}

function holdButton(tg, held) {
  return held === tg
    ? h('button', { class: 'btn small', type: 'button', text: 'Release', onclick: () => setHold(null) })
    : h('button', { class: 'btn small', type: 'button', text: 'Hold', title: `Follow only TG ${tg}`, onclick: () => setHold(tg) });
}

function metric(label) {
  const k = h('span', { class: 'm-label', text: label });
  const v = h('span', { class: 'm-value' });
  return { el: h('div', { class: 'metric' }, k, v), set: t => setText(v, t ?? DASH) };
}

// The channel as the control channel names it: "1-117", "6 · slot 2".
function channel(x) {
  return [x.channel || '', x.slot ? `slot ${x.slot}` : ''].filter(Boolean).join(' · ');
}

// The system card: what is live and its health; the lists make another site live.
function systemCard() {
  const sysPick = h('select', { class: 'input pick', 'aria-label': 'System' });
  const sitePick = h('select', { class: 'input pick', 'aria-label': 'Site' });
  const dot = h('span', { class: 'dot' });
  const state = h('span', { class: 'dim' });
  const hold = h('div', { class: 'row' });
  const head = h('div', { class: 'card-head' }, sysPick, sitePick, dot, state, h('div', { class: 'spacer' }), hold);
  const facts = h('div', { class: 'now-facts' });
  const metrics = h('div', { class: 'site-metrics' });
  const el = h('section', { class: 'card now-site' }, head, facts, metrics);

  let s = null;
  // A system picked whose sites are not live: its sites are offered until one is made live.
  let picked = null;
  let busy = false;
  let drawnPicks = null;
  let drawnFacts = null;
  let drawnHold;
  let drawnMetrics = null;
  let cells = [];

  const systems = () => (s && s.systems) || [];
  const live = () => {
    const l = s && s.status && s.status.live;
    return l && l.state === 'live' ? l : null;
  };
  const siteLabel = id => {
    for (const x of systems()) for (const y of x.sites) if (y.id === id) return y.label;
    return id;
  };

  async function activate(site) {
    busy = true;
    draw(s);
    try {
      await api.activate(site.id);
      picked = null;
      toast(`${site.label} is live`);
      await refresh();
    } catch (e) {
      toast(e.message, true);
    } finally {
      busy = false;
      draw(s);
    }
  }

  sysPick.addEventListener('change', () => {
    picked = sysPick.value;
    const sys = systems().find(x => x.id === picked);
    const liveId = live() && live().site.id;
    if (sys && sys.sites.length === 1 && sys.sites[0].id !== liveId) activate(sys.sites[0]);
    else draw(s);
  });
  sitePick.addEventListener('change', () => {
    const sys = systems().find(x => x.id === sysPick.value);
    const site = sys && sys.sites.find(y => y.id === sitePick.value);
    if (site) activate(site);
  });

  function drawPicks(state) {
    const list = systems();
    const l = live();
    const liveId = l ? l.site.id : null;
    if (picked && !list.some(x => x.id === picked)) picked = null;
    const sys = list.find(x => x.id === picked) || list.find(x => x.sites.some(y => y.id === liveId)) || list[0];
    const locked = busy || state === 'switching' || state === 'scanning';
    const key = JSON.stringify([list.map(x => [x.id, x.label, x.sites.map(y => [y.id, y.label, y.control.freq_hz])]), liveId, sys && sys.id, locked]);
    if (key === drawnPicks) return;
    drawnPicks = key;
    sysPick.hidden = sitePick.hidden = !list.length;
    sysPick.replaceChildren(...list.map(x => h('option', { value: x.id, text: x.label })));
    sysPick.value = sys ? sys.id : '';
    const onLive = !!sys && sys.sites.some(y => y.id === liveId);
    sitePick.replaceChildren(
      ...(onLive ? [] : [h('option', { value: '', text: sys && sys.sites.length ? 'Pick a site' : 'No sites' })]),
      ...(sys ? sys.sites : []).map(y => h('option', { value: y.id, text: `${y.label} · ${mhz(y.control.freq_hz)}` })));
    sitePick.value = onLive ? liveId : '';
    sysPick.disabled = sitePick.disabled = locked;
  }

  function drawState(lv) {
    let text = '';
    let cls = '';
    if (!lv || lv.state === 'no_site') {
      text = systems().length ? 'No site is live: pick one to listen to.' : '';
    } else if (lv.state === 'switching') {
      text = `Switching to ${siteLabel(lv.to)}…`;
    } else if (lv.state === 'scanning') {
      text = 'Scanning for systems: the live site is paused.';
    } else {
      const c = s.status.control || {};
      const age = c.last_message_age_ms;
      const heard = c.running && known(age) && age < SILENT_MS;
      cls = heard ? ' ok' : c.running ? ' warn' : '';
      text = heard ? 'Decoding' : c.running ? `No messages ${known(age) ? `for ${dur(age)}` : 'yet'}` : 'Stopped';
    }
    dot.className = `dot${cls}`;
    dot.hidden = !text;
    setText(state, text);
  }

  // The live system's details and the identities, once per site and edit.
  function drawFacts() {
    const l = live();
    const list = systems();
    const sys = l && list.find(x => x.id === l.system.id);
    const key = JSON.stringify([list.length, l && l.site, l && l.system, sys && [sys.details, sys.identity]]);
    if (key === drawnFacts) return;
    drawnFacts = key;
    if (!list.length && s.systems) {
      facts.replaceChildren(h('span', { text: 'No systems yet.' }), h('a', { class: 'btn small primary', href: '#systems', text: 'Scan for systems' }));
      return;
    }
    if (!l) {
      facts.replaceChildren();
      return;
    }
    const p = protocol(l.system.protocol);
    const d = (sys && sys.details) || {};
    const text = [d.location, d.county, d.system_type || p.label, d.voice].filter(Boolean);
    const sysId = sys && sys.identity && Object.values(sys.identity).some(known) ? p.systemIdentity(sys.identity) : [];
    const ids = [...sysId, ...p.siteIdentity(l.site.identity || {})].filter(([, v]) => known(v));
    facts.replaceChildren(
      ...text.map(t => h('span', { text: t })),
      ...ids.map(([k, v]) => h('span', { class: 'nowrap' }, h('span', { class: 'k', text: k }), String(v))));
  }

  // The control channel's health, each status.
  function drawMetrics() {
    const l = live();
    if (!l) {
      metrics.replaceChildren();
      drawnMetrics = null;
      return;
    }
    const p = protocol(l.system.protocol);
    const c = s.status.control || {};
    const t = l.tuning || {};
    const rows = [
      ['Control channel', mhz(l.site.control.freq_hz)],
      ['Messages', known(c.msgs_per_s) ? `${num(c.msgs_per_s, 1)} /s` : DASH],
      [p.okLabel, pct(c.ok_pct)],
      ['Last message', ago(c.last_message_age_ms)],
      ...p.healthRows(c),
      ['Grants', num(c.grants)],
      ['Window', `${t.preset || DASH} at ${mhz(t.lo_hz)}`],
      ['CPU', pct(c.cpu_pct, 0)],
    ];
    const key = rows.map(r => r[0]).join('|');
    if (key !== drawnMetrics) {
      drawnMetrics = key;
      cells = rows.map(([k]) => metric(k));
      metrics.replaceChildren(...cells.map(x => x.el));
    }
    rows.forEach(([, v], i) => cells[i].set(v));
  }

  function drawHold() {
    const held = (live() && s.status.hold) || null;
    if (held === drawnHold) return;
    drawnHold = held;
    hold.replaceChildren(...(held ? [h('span', { class: 'badge hang', text: `Holding TG ${held}` }), holdButton(held, held)] : []));
  }

  function draw(store) {
    s = store;
    if (!s || !s.status) return;
    const lv = s.status.live;
    drawPicks(lv && lv.state);
    drawState(lv);
    drawFacts();
    drawMetrics();
    drawHold();
  }

  return { el, draw };
}

// The talkgroups a lane can be held on: the live system's named ones and those heard lately,
// by name, then number.
function talkgroups(s) {
  const lv = s.status.live;
  const sys = lv && lv.state === 'live' && (s.systems || []).find(x => x.id === lv.system.id);
  const out = new Map();
  for (const a of (sys && sys.aliases) || []) {
    for (const id of a.ids || []) if (id.type === 'talkgroup') out.set(id.value, a.name);
  }
  for (const c of s.calls ? [...s.calls.open, ...s.calls.recent] : []) {
    if (!c.private && !out.has(c.tg)) out.set(c.tg, c.tg_name || null);
  }
  return [...out].sort((a, b) => (a[1] || '￿').localeCompare(b[1] || '￿') || a[0] - b[0]);
}

function idleLine(s, ch) {
  const lv = s.status.live;
  if (!lv || lv.state !== 'live') return 'No site is live.';
  // A lane the live site does not run (not every lane decodes every protocol).
  if (!ch) return 'Not used at this site.';
  const parts = [];
  if (ch.on_data_channel) parts.push(`Parked on the data channel, ${mhz(ch.tuned_hz)}.`);
  const last = s.calls && s.calls.recent.find(x => x.lane === ch.lane);
  if (last) parts.push(`Last: ${tgName(last)} from ${unitLabel(last.source, last.source_name)}, ended ${ago(boardNow(s) - last.ended_unix_ms)}.`);
  return parts.join(' ') || 'Waiting for a grant.';
}

// One traffic channel: its call, or idle; and the talkgroup it is held on.
function trafficCard(lane) {
  const c = card(`${SIDE[lane]} · Traffic ${lane}`, { class: 'call-card' });
  const flags = h('span', { class: 'row' });
  const phase = h('span', { class: 'badge' });
  const pick = h('select', { class: 'input lane-hold', 'aria-label': `Hold traffic ${lane} on a talkgroup`, title: 'Follow only this talkgroup on this channel (the other channel follows as before)' });
  pick.addEventListener('change', async () => {
    const tg = pick.value ? Number(pick.value) : null;
    try {
      await api.setHold(tg, lane);
      toast(tg === null ? `Traffic ${lane} follows every talkgroup` : `Traffic ${lane} holds TG ${tg}`);
      await refresh();
    } catch (e) {
      toast(e.message, true);
    }
  });
  c.right.append(flags, phase, pick);
  let drawnPick = null;

  // Rebuilt when the list or the hold changed, never under an open list.
  function drawPick(s) {
    const lv = s.status.live;
    pick.disabled = !lv || lv.state !== 'live';
    const held = (s.status.lane_holds || [])[lane - 1] ?? null;
    const list = talkgroups(s);
    const key = JSON.stringify([held, list, pick.disabled]);
    if (key === drawnPick || document.activeElement === pick) return;
    drawnPick = key;
    const opts = [h('option', { value: '', text: 'Any talkgroup' })];
    if (held !== null && !list.some(([tg]) => tg === held)) opts.push(h('option', { value: String(held), text: `TG ${held}` }));
    for (const [tg, name] of list) opts.push(h('option', { value: String(tg), text: name ? `${name} (${tg})` : `TG ${tg}` }));
    pick.replaceChildren(...opts);
    pick.value = held === null ? '' : String(held);
    setClass(pick, 'held', held !== null);
  }
  const tg = h('div', { class: 'call-tg' });
  const sub = h('div', { class: 'call-alias' });
  const src = h('div', { class: 'call-src' });
  const holdSlot = h('div', { class: 'call-hold' });
  const m = { freq: metric('Frequency'), channel: metric('Channel'), elapsed: metric('Elapsed'), voice: metric('Voice') };
  const active = h('div', null, tg, sub, src, h('div', { class: 'call-facts' }, ...Object.values(m).map(x => x.el), holdSlot));
  const idleText = h('div');
  const idle = h('div', { class: 'call-idle' }, h('div', { class: 'big', text: 'Idle' }), idleText);
  c.body.append(active, idle);
  let drawnFlags = null;
  let drawnHold = null;

  function draw(s) {
    drawPick(s);
    const ch = ((s.traffic && s.traffic.channels) || []).find(x => x.lane === lane);
    const call = ch && ch.call;
    active.hidden = !call;
    idle.hidden = !!call;
    c.el.classList.remove('voice', 'hang', 'acquiring');
    if (!call) {
      phase.hidden = true;
      drawnFlags = null;
      drawnHold = null;
      flags.replaceChildren();
      holdSlot.replaceChildren();
      setText(idleText, idleLine(s, ch));
      return;
    }
    const lastVoice = known(ch.last_voice_ms_ago) ? ch.last_voice_ms_ago + (Date.now() - s.trafficAt) : null;
    const ph = call.end_lc ? 'ending'
      : !call.first_voice_unix_ms ? 'acquiring'
        : lastVoice !== null && lastVoice <= VOICE_HOLD_MS ? 'voice' : 'hang';
    const look = ph === 'ending' ? 'hang' : ph;
    c.el.classList.add(look);
    phase.hidden = false;
    phase.className = `badge ${look}`;
    setText(phase, PHASE[ph]);
    const f = [call.emergency && ['bad', 'Emergency'], call.private && ['', 'Private'], call.encrypted && ['enc', 'Encrypted']].filter(Boolean);
    const fk = JSON.stringify(f);
    if (fk !== drawnFlags) {
      drawnFlags = fk;
      flags.replaceChildren(...f.map(([cls, t]) => h('span', { class: `badge ${cls}`, text: t })));
    }
    setText(tg, tgName(call));
    setText(sub, call.tg_name ? `TG ${call.tg}` : '');
    setText(src, `From ${unitLabel(call.source, call.source_name)}`);
    m.freq.set(mhz(call.freq_hz));
    m.channel.set(channel(call) || DASH);
    m.elapsed.set(dur(Math.max(0, boardNow(s) - call.started_unix_ms)));
    m.voice.set(call.voice_frames ? dur(call.voice_frames * 20) : 'none yet');
    const held = s.status.hold || null;
    const hk = `${call.tg}|${held}`;
    if (hk !== drawnHold) {
      drawnHold = hk;
      holdSlot.replaceChildren(holdButton(call.tg, held));
    }
  }

  return { el: c.el, draw };
}

function recordingNote(st) {
  if (!st) return '';
  if (!st.enabled) return 'Recording is off (Settings).';
  const where = st.store === 'sd' ? 'SD card' : 'RAM';
  const sd = st.sd_status.state === 'ok' || st.store !== 'sd' ? '' : ` (SD card ${st.sd_status.state.replace('_', ' ')}: saving to RAM)`;
  return `Recording to ${where}${sd}: ${num(st.sd.count + st.ram.count)} kept, ${bytes(st.sd.bytes + st.ram.bytes)}.`;
}

function createRow() {
  const c = {
    when: h('div', { class: 'when c-when' }),
    tg: h('div', { class: 'tg' }),
    tgSub: h('div', { class: 'sub' }),
    src: h('div'),
    srcSub: h('div', { class: 'sub' }),
    freq: h('div', { class: 'num' }),
    freqSub: h('div', { class: 'sub' }),
    dur: h('div', { class: 'dur' }),
    durSub: h('div', { class: 'sub dur' }),
    play: h('div', { class: 'play c-play' }),
    hold: h('div', { class: 'c-hold' }),
  };
  const el = h('div', { class: 'call-row' },
    c.when,
    h('div', { class: 'c-tg' }, c.tg, c.tgSub),
    h('div', { class: 'c-src' }, c.src, c.srcSub),
    h('div', { class: 'c-freq' }, c.freq, c.freqSub),
    h('div', { class: 'c-dur' }, c.dur, c.durSub),
    c.play, c.hold);
  el._c = c;
  return el;
}

// The recording's player: a button until it is played, then the browser's controls (kept while
// the list is redrawn).
function drawPlay(c, r) {
  const key = r ? String(r.id) : '';
  if (c.play.dataset.k === key) return;
  c.play.dataset.k = key;
  if (!r) {
    c.play.replaceChildren();
    return;
  }
  const url = `/api/v1/recordings/${r.id}.wav`;
  const play = h('button', { class: 'btn small', type: 'button', title: `Play the recording (${r.store === 'sd' ? 'SD card' : 'RAM'})` }, icon('play'), dur(r.duration_ms));
  play.addEventListener('click', () => {
    const audio = h('audio', { controls: true, preload: 'auto', src: url });
    play.replaceWith(audio);
    audio.play().catch(() => { /* some phones need another tap */ });
  });
  const save = h('a', { class: 'btn small icon', href: url, download: r.file, title: 'Download the WAV', 'aria-label': 'Download the WAV' }, icon('download'));
  c.play.replaceChildren(play, save);
}

function updateRow(el, x, ctx) {
  const c = el._c;
  const open = !x.ended_unix_ms;
  setClass(el, 'nf', !!x.not_followed);
  setClass(el, 'open', open);
  if (open) {
    if (!c.when.querySelector('.badge')) c.when.replaceChildren(h('span', { class: 'badge voice', text: 'On air' }));
  } else {
    setText(c.when, clock(x.started_unix_ms, ctx.clockValid) || ago(ctx.now - x.started_unix_ms));
  }
  setText(c.tg, tgName(x));
  setText(c.tgSub, [
    x.tg_name ? `TG ${x.tg}` : '',
    x.private ? 'private' : '',
    x.emergency ? 'emergency' : '',
    x.encrypted ? 'encrypted' : x.not_followed ? NOT_FOLLOWED[x.not_followed] || x.not_followed : '',
  ].filter(Boolean).join(' · '));
  setText(c.src, x.source_name || (x.source ? String(x.source) : 'unknown'));
  setText(c.srcSub, x.source_name && x.source ? String(x.source) : '');
  setText(c.freq, mhz(x.freq_hz));
  setText(c.freqSub, [x.channel ? `ch ${channel(x)}` : '', x.lane ? (SIDE[x.lane] || '').toLowerCase() : ''].filter(Boolean).join(' · '));
  setText(c.dur, x.voice_frames ? dur(x.voice_frames * 20) : DASH);
  const air = known(x.grant_ms) ? x.grant_ms : open ? ctx.now - x.started_unix_ms : x.open_ms;
  setText(c.durSub, air ? `on air ${dur(air)}` : '');
  drawPlay(c, ctx.recs.get(x.call));
  const hk = x.encrypted ? '' : `${x.tg}|${ctx.held}`;
  if (c.hold.dataset.k !== hk) {
    c.hold.dataset.k = hk;
    c.hold.replaceChildren(...(x.encrypted ? [] : [holdButton(x.tg, ctx.held)]));
  }
}

// The live site's calls: those on the air, then the newest closed ones.
function callsPane() {
  const c = card('Calls', { flush: true, class: 'now-calls' });
  const count = h('span', { class: 'dim' });
  const search = h('input', { class: 'input', type: 'search', placeholder: 'Talkgroup or radio', 'aria-label': 'Filter the calls' });
  const nf = h('button', { class: 'chip', type: 'button', text: 'Not followed', title: 'Show the calls no traffic channel followed' });
  c.right.append(count, search, nf);
  const list = h('div', { class: 'calls' });
  const empty = h('div', { class: 'calls-empty' });
  c.body.append(list, empty);
  let s = null;
  let drawn = null;
  search.addEventListener('input', () => draw(s, true));
  nf.addEventListener('click', () => {
    showNotFollowed = !showNotFollowed;
    draw(s, true);
  });

  function draw(store, force) {
    s = store;
    if (!s || !s.status) return;
    const held = s.status.hold || null;
    const key = [s.calls, s.recordings, held];
    if (!force && drawn && key.every((v, i) => v === drawn[i])) return;
    drawn = key;
    setClass(nf, 'on', showNotFollowed);
    const v = s.calls || { open: [], recent: [] };
    const all = [...v.open, ...v.recent];
    const q = search.value.trim().toLowerCase();
    const items = all.filter(x => (showNotFollowed || !x.not_followed)
      && (!q || [x.tg, x.tg_name, x.source, x.source_name].some(f => known(f) && String(f).toLowerCase().includes(q))));
    const recs = new Map(((s.recordings && s.recordings.items) || []).map(r => [r.id, r]));
    const storage = s.recordings && s.recordings.storage;
    const recorded = all.filter(x => recs.has(x.call)).length;
    setText(count, s.calls ? `${num(all.length)} calls · ${num(recorded)} recorded${storage && !storage.enabled ? ' · recording off' : ''}` : '');
    count.title = recordingNote(storage);
    const ctx = { now: boardNow(s), clockValid: !!(s.status.clock && s.status.clock.valid), recs, held };
    keyedList(list, items, x => x.call, createRow, (el, x) => updateRow(el, x, ctx));
    const lv = s.status.live;
    empty.hidden = items.length > 0;
    setText(empty, !s.calls ? 'Loading…' : q || all.length ? 'No matching calls.' : lv && lv.state === 'live' ? 'No calls yet.' : 'No site is live.');
  }

  const ages = setInterval(() => draw(s, true), AGES_MS);
  return { el: c.el, draw, stop: () => clearInterval(ages) };
}

export function mount(el) {
  const sys = systemCard();
  const lanes = [trafficCard(1), trafficCard(2)];
  const calls = callsPane();
  el.append(h('div', { class: 'now' }, sys.el, h('div', { class: 'grid-2 now-traffic' }, ...lanes.map(x => x.el)), calls.el));
  let last = null;
  const tick = setInterval(() => {
    if (last && last.status) for (const l of lanes) l.draw(last);
  }, TICK_MS);

  return {
    update(s) {
      last = s;
      if (!s.status) return;
      sys.draw(s);
      for (const l of lanes) l.draw(s);
      calls.draw(s);
    },
    unmount() {
      clearInterval(tick);
      calls.stop();
    },
  };
}
