// Now: a trunked monitoring station's front, filling the screen. Across the top the site bar
// (the site listened to and whether its control channel decodes, the radio's hold, what is
// recorded) and the two traffic tuners side by side: Traffic 1 and Traffic 2, each with its call
// or what it waits for, the talkgroup it is kept on, its last calls to play again, and this
// browser's volume and mute for it. Under them the talkgroups (talkgroups.js) and the site's
// calls with the picked call's player (now_calls.js). Everything shown is pushed by the radio
// (the store), and every control is one write to it.

import { h, toast, setText, setClass, icon } from '../dom.js';
import { api } from '../api.js';
import { refresh, boardNow } from '../store.js';
import { mhz, pct, num, dur, bytes, unitLabel } from '../format.js';
import { protocol } from '../protocols.js';
import { RECORD } from '../recording.js';
import { player } from '../audio/player.js';
import { getPrefs, setLane, onPrefs } from '../prefs.js';
import { tgLabel, nameOf } from '../aliases.js';
import { talkgroupList, talkgroupsOf } from './talkgroups.js';
import { callsPane } from './now_calls.js';
import { alertLabel, channel, recordingOf, SIDE } from './call_rows.js';

// A control channel with no message for this long is shown as silent.
const SILENT_MS = 5000;
// Voice this recent: the call is talking; after it, the call hangs until it closes.
const VOICE_HOLD_MS = 1500;
// The tuners' clocks move between pushes.
const TICK_MS = 250;
const PHASE = { acquiring: 'Acquiring', voice: 'Voice', hang: 'Hang', ending: 'Ended' };
// A tuner's last calls offered to play again.
const REPLAYS = 3;

const known = v => v !== null && v !== undefined;
const liveOf = s => {
  const lv = s && s.status && s.status.live;
  return lv && lv.state === 'live' ? lv : null;
};

// Hold the radio on `tg`: only it is followed, on both tuners. Null releases the hold.
async function setHold(tg) {
  try {
    await api.setHold(tg);
    toast(tg === null ? 'Hold released' : `Holding TG ${tg}`);
    await refresh();
  } catch (e) {
    toast(e.message, true);
  }
}

// The site bar: the site listened to (another picked from the list is made live), whether it
// decodes, the hold, and what is recorded.
function siteBar() {
  const pick = h('select', { class: 'input pick', 'aria-label': 'The site listened to' });
  const dot = h('span', { class: 'dot' });
  const state = h('span', { class: 'site-state' });
  const health = h('span', { class: 'dim' });
  const hold = h('span', { class: 'row' });
  const none = h('span', { class: 'row', hidden: true }, h('span', { text: 'No systems yet.' }), h('a', { class: 'btn small primary', href: '#systems', text: 'Scan for systems' }));
  const rec = RECORD.map(([key, text, title]) => h('button', { type: 'button', text, title, dataset: { key } }));
  const recSeg = h('div', { class: 'seg', role: 'group', 'aria-label': 'What is recorded' }, ...rec);
  const stored = h('span', { class: 'dim' });
  const el = h('section', { class: 'card now-site', 'aria-label': 'Live site' },
    pick, none, h('span', { class: 'row' }, dot, state), health, hold, h('div', { class: 'spacer' }),
    h('span', { class: 'dim', text: 'Record' }), recSeg, stored);

  let s = null;
  let busy = false;
  let drawnPick = null;
  let drawnHold;

  const systems = () => (s && s.systems) || [];
  const siteOf = id => {
    for (const x of systems()) for (const y of x.sites) if (y.id === id) return y;
    return null;
  };

  pick.addEventListener('change', async () => {
    const site = siteOf(pick.value);
    // Out of the list, so it shows the live site again when the radio refuses.
    pick.blur();
    drawnPick = null;
    if (!site) return;
    busy = true;
    draw(s);
    try {
      await api.activate(site.id);
      toast(`${site.label} is live`);
      await refresh();
    } catch (e) {
      toast(e.message, true);
    } finally {
      busy = false;
      draw(s);
    }
  });

  for (const b of rec) {
    b.addEventListener('click', async () => {
      const [, , , body] = RECORD.find(x => x[0] === b.dataset.key);
      try {
        await api.setRecording(body);
        const marked = talkgroupsOf(s).filter(t => t.record).length;
        toast(b.dataset.key === 'marked' ? `Recording the talkgroups marked Rec: ${num(marked)} now` : b.dataset.key === 'every' ? 'Recording every followed call' : 'Recording is off');
      } catch (e) {
        toast(e.message, true);
      }
    });
  }

  // The sites by system; never rebuilt under an open list.
  function drawPick(st) {
    const list = systems();
    const lv = liveOf(s);
    const liveId = lv ? lv.site.id : null;
    // A switch, a scan or another module has the radio: no site can be made live meanwhile.
    const locked = busy || (st !== 'live' && st !== 'no_site');
    pick.disabled = locked;
    pick.hidden = !list.length;
    none.hidden = !!list.length || !s.systems;
    if (document.activeElement === pick) return;
    const key = JSON.stringify([list.map(x => [x.id, x.label, x.sites.map(y => [y.id, y.label, y.control.freq_hz])]), liveId]);
    if (key === drawnPick) return;
    drawnPick = key;
    pick.replaceChildren(
      ...(liveId ? [] : [h('option', { value: '', text: 'No site is live: pick one' })]),
      ...list.map(x => h('optgroup', { label: x.label }, ...x.sites.map(y => h('option', { value: y.id, text: `${x.label} › ${y.label} · ${mhz(y.control.freq_hz)}` })))));
    pick.value = liveId || '';
  }

  // Whether the control channel decodes, and how well.
  function drawState(lv) {
    let text = '';
    let cls = '';
    let more = '';
    health.title = '';
    if (!s.connected) {
      text = 'Not connected to the radio: this is how it was.';
      cls = ' warn';
    } else if (!lv || lv.state === 'no_site') {
      text = systems().length ? 'No site is live.' : '';
    } else if (lv.state === 'switching') {
      const to = siteOf(lv.to);
      text = `Switching to ${to ? to.label : lv.to}…`;
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

  function drawHold() {
    const held = (liveOf(s) && s.status.hold) || null;
    if (held === drawnHold) return;
    drawnHold = held;
    const t = held && talkgroupsOf(s).find(x => x.tg === held);
    hold.replaceChildren(...(held ? [
      h('span', { class: 'badge hang', text: `Holding ${t ? t.label : `TG ${held}`}` }),
      h('button', { class: 'btn small', type: 'button', text: 'Release', onclick: () => setHold(null) }),
    ] : []));
  }

  // What is recorded, and what the card holds.
  function drawRecord() {
    const p = s.talkgroups && s.talkgroups.recording;
    const now = !p ? null : !p.enabled ? 'off' : p.every_call ? 'every' : 'marked';
    for (const b of rec) {
      const on = b.dataset.key === now;
      setClass(b, 'on', on);
      b.setAttribute('aria-pressed', String(on));
      b.disabled = !p;
    }
    const st = s.recordings && s.recordings.storage;
    const free = st && st.sd_status && known(st.sd_status.free_bytes) ? st.sd_status.free_bytes : null;
    const where = st && st.store === 'sd' ? 'the card' : 'RAM';
    setText(stored, s.recordings ? [`${num(s.recordings.total)} recorded`, st && st.store === 'sd' && free !== null ? `${bytes(free)} free on ${where}` : st ? `to ${where}` : ''].filter(Boolean).join(' · ') : '');
    stored.title = st && st.sd_status && st.sd_status.state !== 'ok' && st.store === 'sd' ? `SD card ${st.sd_status.state.replace('_', ' ')}: saving to RAM` : 'Recordings of this site';
    setClass(stored, 'warn', !!st && st.store === 'sd' && !!st.sd_status && st.sd_status.state !== 'ok');
  }

  function draw(store) {
    s = store;
    if (!s || !s.status) return;
    const lv = s.status.live;
    drawPick(lv ? lv.state : 'no_site');
    drawState(lv);
    drawHold();
    drawRecord();
  }

  return { el, draw };
}

// The talkgroups a tuner can be kept on: those not heard only encrypted (a tuner kept on one of
// those would never play).
const keepable = s => talkgroupsOf(s).filter(t => t.kind !== 'enc').map(t => [t.tg, t.name]);

function idleLine(s, ch) {
  const lv = s.status.live;
  if (lv && lv.state === 'away') return 'The scanner is paused.';
  if (!lv || lv.state !== 'live') return 'No site is live.';
  // A lane the live site does not run (not every lane decodes every protocol).
  if (!ch) return 'Not used at this site.';
  if (ch.on_data_channel) return `Waiting on the data channel, ${mhz(ch.tuned_hz)}.`;
  return 'Waiting for a grant.';
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

// One traffic tuner: its call, or what it waits for; the radio's hold on the call it carries;
// the talkgroup it is kept on; its last calls, to play again (`play(call)`); its audio here.
function tunerCard(lane, play) {
  const audio = laneAudio(lane);
  // The newest alert tone the player heard on this lane, while it is the lane's call.
  let alerted = null;
  const offAlert = player.onAlert(a => {
    if (a.lane === lane - 1) alerted = a;
  });
  const kicker = h('span', { class: 'kicker' });
  const flags = h('span', { class: 'row' });
  const phase = h('span', { class: 'badge' });
  const keepOn = h('select', { class: 'input tuner-keep', 'aria-label': `Keep Traffic ${lane} on a talkgroup`, title: 'Follow only this talkgroup on this tuner (the other tuner follows as before)' });
  keepOn.addEventListener('change', async () => {
    const tg = keepOn.value ? Number(keepOn.value) : null;
    // Out of the list, so it shows what the radio then holds: the pick, or what was there when
    // the radio refuses.
    keepOn.blur();
    drawnKeep = null;
    try {
      await api.setHold(tg, lane);
      toast(tg === null ? `Traffic ${lane} follows any talkgroup` : `Traffic ${lane} follows only TG ${tg}`);
      await refresh();
    } catch (e) {
      toast(e.message, true);
    }
  });
  let drawnKeep = null;
  let split = false;
  const offPrefs = onPrefs(p => {
    split = p.split;
  });

  // Rebuilt when the list or the kept talkgroup changed, never under an open list.
  function drawKeep(s) {
    keepOn.disabled = !liveOf(s);
    if (document.activeElement === keepOn) return;
    const held = (s.status.lane_holds || [])[lane - 1] ?? null;
    const list = keepable(s);
    const key = JSON.stringify([held, list]);
    if (key === drawnKeep) return;
    drawnKeep = key;
    const opts = [h('option', { value: '', text: 'Follows any talkgroup' })];
    if (held !== null && !list.some(([tg]) => tg === held)) opts.push(h('option', { value: String(held), text: `Only TG ${held}` }));
    for (const [tg, name] of list) opts.push(h('option', { value: String(tg), text: name ? `Only ${name} (${tg})` : `Only TG ${tg}` }));
    keepOn.replaceChildren(...opts);
    keepOn.value = held === null ? '' : String(held);
    setClass(keepOn, 'held', held !== null);
  }

  const dot = h('span', { class: 'tg-dot' });
  const tg = h('span', { class: 'tuner-name' });
  const sub = h('div', { class: 'tuner-sub' });
  const clock = h('div', { class: 'tuner-clock' });
  const clockSub = h('div', { class: 'sub' });
  const holdSlot = h('span');
  const before = h('span', { class: 'tuner-before' });
  const el = h('section', { class: 'card tuner', 'aria-label': `Traffic ${lane}` },
    h('div', { class: 'row' }, kicker, phase, flags, h('div', { class: 'spacer' }), audio.el),
    h('div', { class: 'tuner-main' }, h('div', { class: 'tuner-what' }, h('div', { class: 'tuner-tg' }, dot, tg), sub), h('div', { class: 'tuner-when' }, clock, clockSub)),
    h('div', { class: 'tuner-foot' }, holdSlot, keepOn, h('div', { class: 'spacer' }), before));
  let drawnFlags = null;
  let drawnHold = null;
  let drawnBefore = null;

  // The tuner's last calls with a recording, newest first.
  function drawBefore(s, site) {
    const recs = new Map(((s.recordings && s.recordings.items) || []).map(r => [r.id, r]));
    const last = ((s.calls && s.calls.recent) || []).filter(x => x.lane === lane && x.site === site && recordingOf(x, recs)).slice(0, REPLAYS);
    const key = last.map(x => x.call).join();
    if (key === drawnBefore) return;
    drawnBefore = key;
    before.replaceChildren(...(last.length ? [h('span', { class: 'sub', text: 'Before' })] : []), ...last.map(x => {
      const r = recordingOf(x, recs);
      return h('button', {
        class: 'replay', type: 'button', title: `Play ${tgLabel(x.tg, x.tg_name)} from ${unitLabel(x.source, x.source_name)} again`,
        onclick: () => play(x),
      }, icon('play', 12), `${tgLabel(x.tg, x.tg_name)} · ${dur(r.duration_ms)}`);
    }));
  }

  let seenRecent = null;
  let seenRecs = null;

  function draw(s) {
    const lv = liveOf(s);
    const two = ((s.traffic && s.traffic.channels) || []).length > 1;
    setText(kicker, `Traffic ${lane}${split && two ? ` · ${SIDE[lane]}` : ''}`);
    drawKeep(s);
    if (!s.calls || s.calls.recent !== seenRecent || s.recordings !== seenRecs) {
      seenRecent = s.calls && s.calls.recent;
      seenRecs = s.recordings;
      drawBefore(s, lv ? lv.site.id : null);
    }
    const ch = ((s.traffic && s.traffic.channels) || []).find(x => x.lane === lane);
    const call = lv && ch && ch.call;
    el.classList.remove('voice', 'hang', 'acquiring');
    setClass(el, 'idle', !call);
    if (!call) {
      phase.className = 'badge';
      setText(phase, 'Idle');
      drawnFlags = null;
      drawnHold = null;
      flags.replaceChildren();
      holdSlot.replaceChildren();
      dot.hidden = true;
      setText(tg, 'Waiting for a call');
      setText(sub, idleLine(s, ch));
      setText(clock, '');
      setText(clockSub, '');
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
    const listed = talkgroupsOf(s).find(t => t.tg === call.tg);
    dot.hidden = false;
    dot.style.background = listed ? listed.color : 'var(--idle)';
    setText(tg, tgLabel(call.tg, call.tg_name));
    setText(sub, [nameOf(call.tg, call.tg_name) ? `TG ${call.tg}` : '', `from ${unitLabel(call.source, call.source_name)}`, [mhz(call.freq_hz), channel(call)].filter(Boolean).join(' ')].filter(Boolean).join(' · '));
    setText(clock, dur(Math.max(0, boardNow() - call.started_unix_ms)));
    setText(clockSub, call.voice_frames ? `voice ${dur(call.voice_frames * 20)}` : 'no voice yet');
    const held = s.status.hold || null;
    const hk = `${call.tg}|${held}`;
    if (hk !== drawnHold) {
      drawnHold = hk;
      holdSlot.replaceChildren(held === call.tg
        ? h('button', { class: 'btn small lit', type: 'button', text: 'Release', title: 'Follow every talkgroup again', onclick: () => setHold(null) })
        : h('button', { class: 'btn small', type: 'button', text: 'Hold', title: `Follow only ${tgLabel(call.tg, call.tg_name)}, until released`, onclick: () => setHold(call.tg) }));
    }
  }

  return {
    el,
    draw,
    stop() {
      audio.stop();
      offAlert();
      offPrefs();
    },
  };
}

export function mount(el) {
  const calls = callsPane();
  const list = talkgroupList({ hold: setHold });
  const site = siteBar();
  const tuners = [tunerCard(1, calls.play), tunerCard(2, calls.play)];
  const root = h('div', { class: 'now' }, site.el, h('div', { class: 'now-tuners' }, ...tuners.map(x => x.el)), list.el, calls.el);
  el.append(root);
  let last = null;
  // The tuners' clocks, while the radio is heard from.
  const tick = setInterval(() => {
    if (last && last.status && last.connected) for (const t of tuners) t.draw(last);
  }, TICK_MS);
  // The page's keys go to the talkgroup list, away from any field.
  const onKey = e => {
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    if (e.target instanceof Element && e.target.closest('input:not([type=checkbox]), select, textarea')) return;
    if (list.key(e)) e.preventDefault();
  };
  document.addEventListener('keydown', onKey);

  return {
    update(s) {
      last = s;
      if (!s.status) return;
      // With the radio not heard from, the page shows how things were, and says so.
      setClass(root, 'stale', !s.connected);
      site.draw(s);
      for (const t of tuners) t.draw(s);
      list.draw(s);
      calls.draw(s);
    },
    unmount() {
      clearInterval(tick);
      document.removeEventListener('keydown', onKey);
      calls.stop();
      for (const t of tuners) t.stop();
    },
  };
}
