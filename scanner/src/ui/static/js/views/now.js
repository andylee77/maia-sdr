// Now: the live site at a glance, filling the screen. On the left the talkgroup bank
// (talkgroups.js). In the middle the site strip (the system and the site to listen to, picked
// from two lists, and whether its control channel decodes), the two traffic channels side by
// side (lane 1 on the left speaker, lane 2 on the right, each with this browser's volume and
// mute for it) and the site's calls. On the right the picked call (call_rows.js). The control
// channel's counters are on Diagnostics. Everything comes from the store, which the radio pushes.

import { h, toast, setText, setClass, icon } from '../dom.js';
import { api } from '../api.js';
import { refresh } from '../store.js';
import { mhz, ago, pct, num, dur, unitLabel, DASH } from '../format.js';
import { protocol } from '../protocols.js';
import { player } from '../audio/player.js';
import { getPrefs, setLane, onPrefs } from '../prefs.js';
import { tgLabel, nameOf } from '../aliases.js';
import { talkgroupBank, knownTalkgroups, heardVersion } from './talkgroups.js';
import { callsPane, boardNow } from './now_calls.js';
import { alertLabel, channel, SIDE } from './call_rows.js';

// A control channel with no message for this long is shown as silent.
const SILENT_MS = 5000;
// Voice this recent: the call is talking; after it, the call hangs until it closes.
const VOICE_HOLD_MS = 1500;
// The traffic cards' clocks move between pushes.
const TICK_MS = 250;
const PHASE = { acquiring: 'Acquiring', voice: 'Voice', hang: 'Hang', ending: 'Ended' };

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

function fact(label) {
  const v = h('span', { class: 'fact-v' });
  return { el: h('div', { class: 'fact' }, h('span', { class: 'fact-k', text: label }), v), set: t => setText(v, t ?? DASH) };
}

// The site strip: what is live and whether it decodes; the lists make another site live.
function siteStrip() {
  const sysPick = h('select', { class: 'input pick', 'aria-label': 'System' });
  const sitePick = h('select', { class: 'input pick', 'aria-label': 'Site' });
  const dot = h('span', { class: 'dot' });
  const state = h('span', { class: 'site-state' });
  const health = h('span', { class: 'dim' });
  const hold = h('span', { class: 'row' });
  const facts = h('span', { class: 'site-facts' });
  const el = h('section', { class: 'card now-site', 'aria-label': 'Live site' }, sysPick, sitePick, h('span', { class: 'row' }, dot, state), health, hold, facts);

  let s = null;
  // A system picked whose sites are not live: its sites are offered until one is made live.
  let picked = null;
  let busy = false;
  let drawnPicks = null;
  let drawnFacts = null;
  let drawnHold;

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

  function drawPicks(st) {
    const list = systems();
    const l = live();
    const liveId = l ? l.site.id : null;
    if (picked && !list.some(x => x.id === picked)) picked = null;
    const sys = list.find(x => x.id === picked) || list.find(x => x.sites.some(y => y.id === liveId)) || list[0];
    // A switch, a scan or another module has the radio: no site can be made live meanwhile.
    const locked = busy || (st !== 'live' && st !== 'no_site');
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

  // Whether the control channel decodes, and how well.
  function drawState(lv) {
    let text = '';
    let cls = '';
    let more = '';
    health.title = '';
    if (!lv || lv.state === 'no_site') {
      text = systems().length ? 'No site is live: pick one to listen to.' : '';
    } else if (lv.state === 'switching') {
      text = `Switching to ${siteLabel(lv.to)}…`;
    } else if (lv.state === 'scanning') {
      text = 'Scanning for systems: the live site is paused.';
    } else if (lv.state === 'away') {
      text = 'Paused: another module has the radio.';
    } else {
      const c = s.status.control || {};
      const age = c.last_message_age_ms;
      const heard = c.running && known(age) && age < SILENT_MS;
      cls = heard ? ' ok' : c.running ? ' warn' : '';
      text = heard ? 'Decoding' : c.running ? `No messages ${known(age) ? `for ${dur(age)}` : 'yet'}` : 'Stopped';
      more = [known(c.ok_pct) ? `control channel ${pct(c.ok_pct, 0)}` : '', known(c.msgs_per_s) ? `${num(c.msgs_per_s, 0)} messages a second` : ''].filter(Boolean).join(' · ');
      health.title = `${protocol(lv.system.protocol).okLabel}; the counters are on Diagnostics`;
    }
    dot.className = `dot${cls}`;
    dot.hidden = !text;
    setText(state, text);
    setText(health, more);
  }

  // The live system's type and the identities, once per site and edit.
  function drawFacts() {
    const l = live();
    const list = systems();
    const sys = l && list.find(x => x.id === l.system.id);
    const key = JSON.stringify([list.length, !!s.systems, l && l.site, l && l.system, sys && [sys.details, sys.identity]]);
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
    const sysId = sys && sys.identity && Object.values(sys.identity).some(known) ? p.systemIdentity(sys.identity) : [];
    const ids = [...sysId, ...p.siteIdentity(l.site.identity || {})].filter(([, v]) => known(v));
    facts.replaceChildren(
      h('span', { text: d.system_type || p.label }),
      ...ids.map(([k, v]) => h('span', { class: 'nowrap' }, h('span', { class: 'k', text: k }), String(v))));
    facts.title = [d.location, d.county, d.voice].filter(Boolean).join(' · ');
  }

  function drawHold() {
    const held = (live() && s.status.hold) || null;
    if (held === drawnHold) return;
    drawnHold = held;
    hold.replaceChildren(...(held ? [
      h('span', { class: 'badge hang', text: `Holding TG ${held}` }),
      h('button', { class: 'btn small', type: 'button', text: 'Release', onclick: () => setHold(null) }),
    ] : []));
  }

  function draw(store) {
    s = store;
    if (!s || !s.status) return;
    const lv = s.status.live;
    drawPicks(lv ? lv.state : 'no_site');
    drawState(lv);
    drawFacts();
    drawHold();
  }

  return { el, draw };
}

// The talkgroups a traffic channel can be kept on, by name, then number: the known ones heard in
// the clear or not heard at all (a channel kept on an encrypted one would never play).
const keepable = s => knownTalkgroups(s).filter(t => !t.encrypted).map(t => [t.tg, t.name]);

function idleLine(s, ch) {
  const lv = s.status.live;
  if (lv && lv.state === 'away') return 'The scanner is paused.';
  if (!lv || lv.state !== 'live') return 'No site is live.';
  // A lane the live site does not run (not every lane decodes every protocol).
  if (!ch) return 'Not used at this site.';
  const parts = [];
  if (ch.on_data_channel) parts.push(`Waiting on the data channel, ${mhz(ch.tuned_hz)}.`);
  const last = s.calls && s.calls.recent.find(x => x.lane === ch.lane);
  if (last) parts.push(`Last: ${tgLabel(last.tg, last.tg_name)} from ${unitLabel(last.source, last.source_name)}, ${ago(boardNow(s) - last.ended_unix_ms)}.`);
  return parts.join(' ') || 'Waiting for a grant.';
}

// This browser's volume and mute for a lane (prefs.js); moving the volume unmutes it.
function laneAudio(lane) {
  const i = lane - 1;
  const mute = h('button', { class: 'btn small icon', type: 'button' });
  const slider = h('input', { class: 'vol', type: 'range', min: '0', max: '100', step: '5', 'aria-label': `Traffic ${lane} volume` });
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
    slider.title = `Traffic ${lane} volume ${slider.value} %${l.muted ? ' (muted)' : ''}`;
  });
  return { el: h('span', { class: 'lane-audio' }, mute, slider), stop };
}

// One traffic channel: its call, or what it waits for; the talkgroup it is kept on; its audio
// here. The site's hold is offered on the call it carries, and nowhere else on the page.
function trafficCard(lane) {
  const audio = laneAudio(lane);
  // The newest alert tone the player heard on this lane, while it is the lane's call.
  let alerted = null;
  const offAlert = player.onAlert(a => {
    if (a.lane === lane - 1) alerted = a;
  });
  const flags = h('span', { class: 'row' });
  const phase = h('span', { class: 'badge' });
  const pick = h('select', { class: 'input lane-hold', 'aria-label': `Keep traffic ${lane} on a talkgroup`, title: 'Follow only this talkgroup on this channel (the other channel follows as before)' });
  pick.addEventListener('change', async () => {
    const tg = pick.value ? Number(pick.value) : null;
    try {
      await api.setHold(tg, lane);
      toast(tg === null ? `Traffic ${lane} follows every talkgroup` : `Traffic ${lane} follows only TG ${tg}`);
      await refresh();
    } catch (e) {
      toast(e.message, true);
    }
  });
  // What the list was last worked out from, and what it was last drawn as.
  let pickSeen = [];
  let drawnPick = null;

  // Rebuilt when the list or the kept talkgroup changed, never under an open list.
  function drawPick(s) {
    const lv = s.status.live;
    pick.disabled = !lv || lv.state !== 'live';
    if (document.activeElement === pick) return;
    const held = (s.status.lane_holds || [])[lane - 1] ?? null;
    const seen = [s.calls, s.systems, heardVersion(), held];
    if (seen.every((v, i) => v === pickSeen[i])) return;
    pickSeen = seen;
    const list = keepable(s);
    const key = JSON.stringify([held, list]);
    if (key === drawnPick) return;
    drawnPick = key;
    const opts = [h('option', { value: '', text: 'Any talkgroup' })];
    if (held !== null && !list.some(([tg]) => tg === held)) opts.push(h('option', { value: String(held), text: `TG ${held}` }));
    for (const [tg, name] of list) opts.push(h('option', { value: String(tg), text: name ? `${name} (${tg})` : `TG ${tg}` }));
    pick.replaceChildren(...opts);
    pick.value = held === null ? '' : String(held);
    setClass(pick, 'held', held !== null);
  }

  const tg = h('div', { class: 'call-tg' });
  const sub = h('div', { class: 'call-sub' });
  const holdSlot = h('span');
  const f = { air: fact('On air'), voice: fact('Voice'), freq: fact('Frequency') };
  const facts = h('div', { class: 'call-facts' }, ...Object.values(f).map(x => x.el));
  const el = h('section', { class: 'card call-card', 'aria-label': `Traffic ${lane}, ${SIDE[lane].toLowerCase()} speaker` },
    h('div', { class: 'card-head' }, h('h2', { text: `Traffic ${lane} · ${SIDE[lane]}` }), flags, h('div', { class: 'spacer' }), phase),
    h('div', { class: 'call-main' }, tg, sub),
    h('div', { class: 'call-foot' }, facts, h('div', { class: 'spacer' }), pick, audio.el, holdSlot));
  let drawnFlags = null;
  let drawnHold = null;

  function draw(s) {
    drawPick(s);
    const ch = ((s.traffic && s.traffic.channels) || []).find(x => x.lane === lane);
    const call = ch && ch.call;
    el.classList.remove('voice', 'hang', 'acquiring');
    setClass(el, 'idle', !call);
    facts.hidden = !call;
    if (!call) {
      phase.className = 'badge';
      setText(phase, 'Idle');
      drawnFlags = null;
      drawnHold = null;
      flags.replaceChildren();
      holdSlot.replaceChildren();
      setText(tg, 'Waiting for a call');
      setText(sub, idleLine(s, ch));
      return;
    }
    const lastVoice = known(ch.last_voice_ms_ago) ? ch.last_voice_ms_ago + (Date.now() - s.trafficAt) : null;
    const ph = call.end_lc ? 'ending'
      : !call.first_voice_unix_ms ? 'acquiring'
        : lastVoice !== null && lastVoice <= VOICE_HOLD_MS ? 'voice' : 'hang';
    const look = ph === 'ending' ? 'hang' : ph;
    el.classList.add(look);
    phase.className = `badge ${look}`;
    setText(phase, PHASE[ph]);
    const alert = alerted && alerted.call_id === call.call ? alerted : null;
    const fl = [
      alert && ['alert', 'Alert', alertLabel(alert)],
      call.emergency && ['bad', 'Emergency'],
      call.private && ['', 'Private'],
      call.encrypted && ['enc', 'Encrypted'],
    ].filter(Boolean);
    const fk = JSON.stringify(fl);
    if (fk !== drawnFlags) {
      drawnFlags = fk;
      flags.replaceChildren(...fl.map(([cls, t, title]) => h('span', { class: `badge ${cls}`, text: t, title })));
    }
    setText(tg, tgLabel(call.tg, call.tg_name));
    setText(sub, [nameOf(call.tg, call.tg_name) ? `TG ${call.tg}` : '', `from ${unitLabel(call.source, call.source_name)}`].filter(Boolean).join(' · '));
    f.air.set(dur(Math.max(0, boardNow(s) - call.started_unix_ms)));
    f.voice.set(call.voice_frames ? dur(call.voice_frames * 20) : 'none yet');
    f.freq.set([mhz(call.freq_hz), channel(call)].filter(Boolean).join(' · '));
    const held = s.status.hold || null;
    const hk = `${call.tg}|${held}`;
    if (hk !== drawnHold) {
      drawnHold = hk;
      holdSlot.replaceChildren(held === call.tg
        ? h('button', { class: 'btn small', type: 'button', text: 'Release', title: 'Follow every talkgroup again', onclick: () => setHold(null) })
        : h('button', { class: 'btn small', type: 'button', text: 'Hold', title: `Follow only TG ${call.tg}`, onclick: () => setHold(call.tg) }));
    }
  }

  return {
    el,
    draw,
    stop() {
      audio.stop();
      offAlert();
    },
  };
}

export function mount(el) {
  const bank = talkgroupBank();
  const site = siteStrip();
  const lanes = [trafficCard(1), trafficCard(2)];
  const calls = callsPane();
  el.append(h('div', { class: 'now' }, bank.el, site.el, h('div', { class: 'now-traffic' }, ...lanes.map(x => x.el)), calls.list, calls.detail));
  let last = null;
  const tick = setInterval(() => {
    if (last && last.status) for (const l of lanes) l.draw(last);
  }, TICK_MS);

  return {
    update(s) {
      last = s;
      if (!s.status) return;
      site.draw(s);
      for (const l of lanes) l.draw(s);
      bank.draw(s);
      calls.draw(s);
    },
    unmount() {
      clearInterval(tick);
      calls.stop();
      bank.stop();
      for (const l of lanes) l.stop();
    },
  };
}
