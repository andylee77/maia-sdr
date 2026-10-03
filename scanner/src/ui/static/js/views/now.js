// Now: the live site at a glance. Fixed at the top: the system card (the system and the site to
// listen to, picked from two lists; the site's details, identity and control channel health) and
// the traffic channels side by side, lane 1 on the left speaker and lane 2 on the right, each
// with this browser's volume and mute for it. Below them every call of the live site scrolls in
// its own pane, newest first (older ones read from the history as the list reaches its end),
// each with its recording, its alert tones as bookmarks and its details. Everything else comes
// from the store, which the radio pushes.

import { h, card, toast, setText, setClass, keyedList, icon, iconButton } from '../dom.js';
import { api } from '../api.js';
import { refresh } from '../store.js';
import { mhz, ago, pct, num, dur, clock, bytes, unitLabel, DASH, NOT_FOLLOWED, CLOSE_REASON } from '../format.js';
import { protocol } from '../protocols.js';
import { player } from '../audio/player.js';
import { getPrefs, setLane, onPrefs } from '../prefs.js';

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
let showAlerts = false;

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

// The channel as the control channel names it ("ch 1-117", "LCN 6 TS2": the name carries its
// timeslot).
function channel(x) {
  if (!x.channel) return '';
  return /^\d/.test(x.channel) ? `ch ${x.channel}` : x.channel;
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

// The talkgroups a lane can be held on, by name, then number: the live system's named ones, those
// the history heard in the last week and those on the air lately; never one heard only
// encrypted (a lane held on it would never play).
function talkgroups(s) {
  const lv = s.status.live;
  const site = lv && lv.state === 'live' ? lv.site.id : null;
  const sys = site && (s.systems || []).find(x => x.id === lv.system.id);
  const out = new Map();
  for (const a of (sys && sys.aliases) || []) {
    for (const id of a.ids || []) if (id.type === 'talkgroup') out.set(id.value, a.name);
  }
  const ours = heard.site === site;
  const clear = new Set();
  const encrypted = new Set(ours ? heard.encrypted : []);
  for (const [tg, name] of ours ? heard.clear : []) {
    if (!out.has(tg)) out.set(tg, name);
    clear.add(tg);
  }
  for (const c of s.calls ? [...s.calls.open, ...s.calls.recent] : []) {
    if (c.private || c.site !== site) continue;
    if (!out.get(c.tg)) out.set(c.tg, c.tg_name || null);
    if (c.encrypted) encrypted.add(c.tg);
    else clear.add(c.tg);
  }
  return [...out].filter(([tg]) => clear.has(tg) || !encrypted.has(tg))
    .sort((a, b) => (a[1] || '￿').localeCompare(b[1] || '￿') || a[0] - b[0]);
}

// The live site's talkgroups in the history's last week: heard in the clear (with their names),
// and heard only encrypted. Read at most every 5 minutes.
const heard = { site: null, clear: [], encrypted: [], at: 0 };

async function readHeard(site) {
  if (!site || (heard.site === site && Date.now() - heard.at < 300000)) return;
  heard.at = Date.now();
  try {
    const d = await api.activity('talkgroups', new URLSearchParams({ site, hours: '168', limit: '1000' }).toString());
    Object.assign(heard, {
      site,
      clear: d.items.filter(t => t.encrypted < t.calls).map(t => [t.tg, t.alias || null]),
      encrypted: d.items.filter(t => t.calls > 0 && t.encrypted === t.calls).map(t => t.tg),
    });
  } catch { /* read again later */ }
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

// This browser's volume and mute for a lane (prefs.js); moving the volume unmutes it.
function laneAudio(lane) {
  const i = lane - 1;
  const mute = h('button', { class: 'btn small icon', type: 'button' });
  const slider = h('input', { class: 'vol', type: 'range', min: '0', max: '200', step: '5', 'aria-label': `Traffic ${lane} volume` });
  mute.addEventListener('click', () => setLane(i, { muted: !getPrefs().lanes[i].muted }));
  slider.addEventListener('input', () => player.setLaneVolume(i, slider.value / 100));
  slider.addEventListener('change', () => setLane(i, { volume: slider.value / 100, muted: false }));
  const stop = onPrefs(p => {
    const l = p.lanes[i];
    const label = l.muted ? `Traffic ${lane} is muted: play it` : `Mute Traffic ${lane}`;
    mute.replaceChildren(icon(l.muted ? 'muted' : 'speaker'));
    mute.title = label;
    mute.setAttribute('aria-label', label);
    mute.setAttribute('aria-pressed', String(l.muted));
    setClass(mute, 'muted', l.muted);
    setClass(slider, 'off', l.muted);
    if (document.activeElement !== slider) slider.value = String(Math.round(l.volume * 100));
    slider.title = `Traffic ${lane} volume ${Math.round(l.volume * 100)} %${l.muted ? ' (muted)' : ''}`;
  });
  return { el: h('span', { class: 'lane-audio' }, mute, slider), stop };
}

// One traffic channel: its call, or idle; the talkgroup it is held on; its audio here.
function trafficCard(lane) {
  const c = card(`${SIDE[lane]} · Traffic ${lane}`, { class: 'call-card' });
  const audio = laneAudio(lane);
  // The newest alert tone the player heard on this lane, while it is the lane's call.
  let alerted = null;
  const offAlert = player.onAlert(a => {
    if (a.lane === lane - 1) alerted = a;
  });
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
  c.right.append(flags, phase, audio.el, pick);
  let drawnPick = null;

  // Rebuilt when the list or the hold changed, never under an open list.
  function drawPick(s) {
    const lv = s.status.live;
    pick.disabled = !lv || lv.state !== 'live';
    const held = (s.status.lane_holds || [])[lane - 1] ?? null;
    readHeard(lv && lv.state === 'live' ? lv.site.id : null);
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
    const alert = alerted && alerted.call_id === call.call ? alerted : null;
    const f = [
      alert && ['alert', 'Alert', alertLabel(alert)],
      call.emergency && ['bad', 'Emergency'],
      call.private && ['', 'Private'],
      call.encrypted && ['enc', 'Encrypted'],
    ].filter(Boolean);
    const fk = JSON.stringify(f);
    if (fk !== drawnFlags) {
      drawnFlags = fk;
      flags.replaceChildren(...f.map(([cls, t, title]) => h('span', { class: `badge ${cls}`, text: t, title })));
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

  return {
    el: c.el,
    draw,
    stop: () => {
      audio.stop();
      offAlert();
    },
  };
}

function recordingNote(st) {
  if (!st) return '';
  if (!st.enabled) return 'Recording is off (Settings).';
  const where = st.store === 'sd' ? 'SD card' : 'RAM';
  const sd = st.sd_status.state === 'ok' || st.store !== 'sd' ? '' : ` (SD card ${st.sd_status.state.replace('_', ' ')}: saving to RAM)`;
  return `Recording to ${where}${sd}: ${num(st.sd.count + st.ram.count)} kept, ${bytes(st.sd.bytes + st.ram.bytes)}.`;
}

// "warble 806.5/1506.0 Hz" (an alert, or a recording's bookmark).
function alertLabel(a) {
  return a.label || `${a.kind.replace('_', '-')} ${a.tones_hz.map(t => t.toFixed(1)).join('/')} Hz`;
}

const callKey = x => `${x.site}|${x.call}|${x.started_unix_ms}`;
// Its recording: the one the call carries (stored calls, closed ones once saved), else the
// newest recordings' (pushed).
const recordingOf = (x, recs) => x.recording || recs.get(x.call) || null;
// Its alert tones: the call's, else its recording's bookmarks.
const alertsOf = (x, rec) => (x.alerts && x.alerts.length ? x.alerts : (rec && rec.bookmarks) || []);

function createRow() {
  const c = {
    when: h('div', { class: 'when c-when' }),
    tg: h('span', { class: 'tg' }),
    flag: h('span', { class: 'c-flag' }),
    tgSub: h('div', { class: 'sub' }),
    src: h('div'),
    srcSub: h('div', { class: 'sub' }),
    freq: h('div', { class: 'num' }),
    freqSub: h('div', { class: 'sub' }),
    dur: h('div', { class: 'dur' }),
    durSub: h('div', { class: 'sub dur' }),
    play: h('div', { class: 'play c-play' }),
    hold: h('span'),
    details: h('dl', { class: 'kv call-details', hidden: true }),
  };
  const el = h('div', { class: 'call-row' },
    c.when,
    h('div', { class: 'c-tg' }, h('div', { class: 'tg-line' }, c.tg, c.flag), c.tgSub),
    h('div', { class: 'c-src' }, c.src, c.srcSub),
    h('div', { class: 'c-freq' }, c.freq, c.freqSub),
    h('div', { class: 'c-dur' }, c.dur, c.durSub),
    c.play,
    h('div', { class: 'c-hold' }, c.hold, iconButton('info', 'Details', {
      onclick: () => {
        c.details.hidden = !c.details.hidden;
        if (!c.details.hidden) drawDetails(c, el._x, el._ctx);
      },
    })),
    c.details);
  el._c = c;
  return el;
}

// The recording's player: a button until it is played, then the browser's controls (kept while
// the list is redrawn); a button per bookmark plays from it.
function drawPlay(c, r) {
  const marks = (r && r.bookmarks) || [];
  const key = r ? `${r.id}|${marks.length}` : '';
  if (c.play.dataset.k === key) return;
  c.play.dataset.k = key;
  if (!r) {
    c.play.replaceChildren();
    return;
  }
  const url = `/api/v1/recordings/${r.id}.wav`;
  const play = h('button', { class: 'btn small', type: 'button', title: `Play the recording (${r.store === 'sd' ? 'SD card' : 'RAM'})` }, icon('play'), dur(r.duration_ms));
  let audio = null;
  const start = at => {
    if (!audio) {
      audio = h('audio', { controls: true, preload: 'auto', src: url });
      play.replaceWith(audio);
    }
    const go = () => {
      if (at !== null) audio.currentTime = at;
      audio.play().catch(() => { /* some phones need another tap */ });
    };
    if (at !== null && audio.readyState < 1) audio.addEventListener('loadedmetadata', go, { once: true });
    else go();
  };
  play.addEventListener('click', () => start(null));
  const jumps = marks.map(b => h('button', {
    class: 'btn small mark', type: 'button', title: `Play from the ${alertLabel(b)}, ${dur(b.offset_ms)} in`,
    onclick: () => start(b.offset_ms / 1000),
  }, icon('bell'), dur(b.offset_ms)));
  const save = h('a', { class: 'btn small icon', href: url, download: r.file, title: 'Download the WAV (its alert tones are cue points)', 'aria-label': 'Download the WAV' }, icon('download'));
  c.play.replaceChildren(play, ...jumps, save);
}

// What is known of a call: who, when, its voice and vocoder counts, its channel time, its
// channel, its recording and alert tones.
function detailRows(x, ctx) {
  const rec = recordingOf(x, ctx.recs);
  const alerts = alertsOf(x, rec);
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
  rows.push(['Channel', [mhz(x.freq_hz), channel(x), known(x.slot) ? `slot ${x.slot}` : '', x.lane ? `Traffic ${x.lane} (${(SIDE[x.lane] || '').toLowerCase()})` : ''].filter(Boolean).join(' · ')]);
  if (x.not_followed) rows.push(['Not followed', NOT_FOLLOWED[x.not_followed] || x.not_followed]);
  const flags = [x.emergency && 'emergency', x.private && 'unit to unit', x.encrypted && 'encrypted'].filter(Boolean);
  if (flags.length) rows.push(['Flags', flags.join(', ')]);
  rows.push(['Recording', rec ? `${rec.file} (${bytes(rec.bytes)}, ${rec.store === 'sd' ? 'SD card' : 'RAM'})` : x.ended_unix_ms ? 'none' : DASH]);
  for (const a of alerts) rows.push(['Alert tone', `${alertLabel(a)}, ${dur(a.offset_ms)} in, for ${dur(a.duration_ms)}`]);
  return rows;
}

function drawDetails(c, x, ctx) {
  if (!x || !ctx) return;
  const rows = detailRows(x, ctx);
  const key = JSON.stringify(rows);
  if (c.details.dataset.k === key) return;
  c.details.dataset.k = key;
  c.details.replaceChildren(...rows.flatMap(([k, v]) => [h('dt', { text: k }), h('dd', { text: v })]));
}

function updateRow(el, x, ctx) {
  const c = el._c;
  el._x = x;
  el._ctx = ctx;
  const open = !x.ended_unix_ms;
  const rec = recordingOf(x, ctx.recs);
  const alerts = alertsOf(x, rec);
  setClass(el, 'nf', !!x.not_followed);
  setClass(el, 'open', open);
  setClass(el, 'alert', alerts.length > 0);
  if (open) {
    if (!c.when.querySelector('.badge')) c.when.replaceChildren(h('span', { class: 'badge voice', text: 'On air' }));
  } else {
    setText(c.when, clock(x.started_unix_ms, ctx.clockValid) || ago(ctx.now - x.started_unix_ms));
  }
  setText(c.tg, tgName(x));
  const flagKey = alerts.map(alertLabel).join(', ');
  if (c.flag.dataset.k !== flagKey) {
    c.flag.dataset.k = flagKey;
    c.flag.replaceChildren(...(alerts.length ? [h('span', { class: 'badge alert', text: 'Alert', title: flagKey })] : []));
  }
  setText(c.tgSub, [
    x.tg_name ? `TG ${x.tg}` : '',
    x.private ? 'private' : '',
    x.emergency ? 'emergency' : '',
    x.encrypted ? 'encrypted' : x.not_followed ? NOT_FOLLOWED[x.not_followed] || x.not_followed : '',
  ].filter(Boolean).join(' · '));
  setText(c.src, x.source_name || (x.source ? String(x.source) : 'unknown'));
  setText(c.srcSub, x.source_name && x.source ? String(x.source) : '');
  setText(c.freq, mhz(x.freq_hz));
  setText(c.freqSub, [channel(x), x.lane ? (SIDE[x.lane] || '').toLowerCase() : ''].filter(Boolean).join(' · '));
  setText(c.dur, x.voice_frames ? dur(x.voice_frames * 20) : DASH);
  const air = known(x.grant_ms) ? x.grant_ms : open ? ctx.now - x.started_unix_ms : x.open_ms;
  setText(c.durSub, air ? `on air ${dur(air)}` : '');
  drawPlay(c, rec);
  const hk = x.encrypted ? '' : `${x.tg}|${ctx.held}`;
  if (c.hold.dataset.k !== hk) {
    c.hold.dataset.k = hk;
    c.hold.replaceChildren(...(x.encrypted ? [] : [holdButton(x.tg, ctx.held)]));
  }
  if (!c.details.hidden) drawDetails(c, x, ctx);
}

// Calls read from the history a page at a time as the list is scrolled to its end.
const PAGE = 100;

// The live site's calls: those on the air, then every closed one, newest first. The radio pushes
// the newest; older ones come from the history as the list reaches its end (all of them, page by
// page). Closed calls stay in the list once seen, so a long session keeps them all.
function callsPane() {
  const c = card('Calls', { flush: true, class: 'now-calls' });
  const count = h('span', { class: 'dim' });
  const search = h('input', { class: 'input', type: 'search', placeholder: 'Talkgroup or radio', 'aria-label': 'Filter the calls' });
  const alertChip = h('button', { class: 'chip', type: 'button', text: 'Alerts', title: 'Show only the calls with an alert tone' });
  const nf = h('button', { class: 'chip', type: 'button', text: 'Not followed', title: 'Show the calls no traffic channel followed' });
  c.right.append(count, search, alertChip, nf);
  const list = h('div', { class: 'calls' });
  const empty = h('div', { class: 'calls-empty' });
  const moreBtn = h('button', { class: 'btn small', type: 'button', text: 'Load older calls' });
  const moreText = h('span');
  const more = h('div', { class: 'calls-more' }, moreText, moreBtn);
  c.body.append(list, empty, more);
  let s = null;
  let drawn = null;
  // This site's closed calls by key, and the history's paging.
  const book = { site: null, closed: new Map(), done: false, loading: false, error: null, total: null, totalAt: 0, version: 0 };
  search.addEventListener('input', () => draw(s, true));
  nf.addEventListener('click', () => {
    showNotFollowed = !showNotFollowed;
    draw(s, true);
  });
  alertChip.addEventListener('click', () => {
    showAlerts = !showAlerts;
    draw(s, true);
  });
  moreBtn.addEventListener('click', () => loadOlder());

  const liveSite = () => {
    const lv = s && s.status && s.status.live;
    return lv && lv.state === 'live' ? lv.site.id : null;
  };
  const filtering = () => !!search.value.trim() || showAlerts;

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
    }
    const held = s.status.hold || null;
    const key = [s.calls, s.recordings, held, book.version];
    if (!force && drawn && key.every((v, i) => v === drawn[i])) return;
    drawn = key;
    if (site) readTotal(site);
    setClass(nf, 'on', showNotFollowed);
    setClass(alertChip, 'on', showAlerts);
    const v = s.calls || { open: [], recent: [] };
    // The pushed calls replace what was read of them (they learn their recordings and alerts).
    for (const x of v.recent) if (x.site === site) book.closed.set(callKey(x), x);
    const openKeys = new Set(v.open.map(callKey));
    const closed = [...book.closed.values()].filter(x => !openKeys.has(callKey(x))).sort((a, b) => b.started_unix_ms - a.started_unix_ms);
    const all = [...v.open, ...closed];
    const recs = new Map(((s.recordings && s.recordings.items) || []).map(r => [r.id, r]));
    const q = search.value.trim().toLowerCase();
    const items = all.filter(x => (showNotFollowed || !x.not_followed)
      && (!showAlerts || alertsOf(x, recordingOf(x, recs)).length > 0)
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
    const ctx = { now: boardNow(s), clockValid: !!(s.status.clock && s.status.clock.valid), recs, held, site };
    keyedList(list, items, callKey, createRow, (el, x) => updateRow(el, x, ctx));
    const lv = s.status.live;
    empty.hidden = items.length > 0;
    setText(empty, !s.calls ? 'Loading…' : q || showAlerts || all.length ? 'No matching calls.' : lv && lv.state === 'live' ? 'No calls yet.' : 'No site is live.');
    more.hidden = !site;
    moreBtn.hidden = book.done || book.loading;
    setText(moreBtn, filtering() ? 'Search older calls' : 'Load older calls');
    setText(moreText, book.error ? `Older calls not read: ${book.error}` : book.loading ? 'Reading older calls…' : book.done ? `All ${num(all.length)} calls of this site.` : '');
  }

  const ages = setInterval(() => draw(s, true), AGES_MS);
  return {
    el: c.el,
    draw,
    stop: () => {
      clearInterval(ages);
      io.disconnect();
    },
  };
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
      for (const l of lanes) l.stop();
    },
  };
}
