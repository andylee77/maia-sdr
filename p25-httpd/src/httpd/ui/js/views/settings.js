// "Settings": recording on/off + storage + retention, call close
// timing, aliases, monitor list, encrypted lockout, clock, display.
// Radio-side settings are persisted on the board
// (/mnt/jffs2/p25-ui-settings.json); display settings live in this
// browser only.

import { h, card, toast, switchInput } from '../dom.js';
import { api } from '../api.js';
import { store, setPref, kick } from '../store.js';
import { bytes, dur, DASH, siteClock, utcOffset } from '../format.js';
import { kvTable } from '../components/kv_table.js';
import { aliasEditor } from '../components/alias_editor.js';
import { monitorPicker } from '../components/monitor_picker.js';
import { tgGroupsEditor } from '../components/tg_groups_editor.js';
import { ignoreList } from '../components/ignore_list.js';
import { profilePicker } from '../components/profile_picker.js';

async function save(patch) {
  const r = await api.putSettings(patch);
  if (!r.persisted) toast('Applied, but NOT saved to flash: ' + (r.save_error || 'no settings file'), true);
  kick(50);
  return r;
}

const num = (attrs, label) => h('input', Object.assign({ class: 'input num', type: 'number', 'aria-label': label }, attrs));
const setIfIdle = (el, v) => { if (document.activeElement !== el) el.value = v; };
const SD_STATE = { ok: 'OK', unknown: 'not checked yet', absent: 'no card mounted', read_only: 'read-only', full: 'full', error: 'write error' };

function recordingCard(onSaved) {
  const c = card('Recording');
  const kv = kvTable();
  const sw = switchInput('Save a recording of every followed call', true, async on => {
    try { await save({ recording: { enabled: on } }); onSaved(); } catch (e) { sw.input.checked = !on; toast(e.message, true); }
  });
  // Change 057: RAM (tmpfs) or SD card for new recordings.
  const where = h('select', { class: 'input', 'aria-label': 'Where to save recordings' },
    h('option', { value: 'ram', text: 'RAM (lost on reboot)' }), h('option', { value: 'sd', text: 'SD card (kept)' }));
  where.addEventListener('change', async () => {
    try { await save({ recording: { storage: where.value } }); toast('New recordings go to ' + (where.value === 'sd' ? 'the SD card' : 'RAM')); onSaved(); } catch (e) { toast(e.message, true); onSaved(); }
  });
  const keep = num({ min: 1, max: 500 }, 'Recordings to keep in RAM');
  const sdKeep = num({ min: 1, max: 5000 }, 'Recordings to keep on the SD card');
  const sdMb = num({ min: 16, max: 32768 }, 'SD card space for recordings, MB');
  const apply = h('button', { class: 'btn small', type: 'button', text: 'Set' });
  apply.addEventListener('click', async () => {
    const p = { max_count: parseInt(keep.value, 10), sd_max_count: parseInt(sdKeep.value, 10), sd_max_mb: parseInt(sdMb.value, 10) };
    try {
      const r = await save({ recording: p });
      toast('Retention saved' + (r.evicted ? ' (' + r.evicted + ' deleted)' : ''));
      onSaved();
    } catch (e) { toast(e.message, true); }
  });
  // Change 065: delete a store's recordings (files and list entries).
  const clearBtn = (label, store, what) => {
    const b = h('button', { class: 'btn small', type: 'button', text: label });
    b.addEventListener('click', async () => {
      if (!window.confirm('Delete ' + what + '? This cannot be undone.')) return;
      try {
        const r = await api.clearRecordings(store);
        toast(r.deleted + ' recording' + (r.deleted === 1 ? '' : 's') + ' deleted');
        onSaved();
        kick(50);
      } catch (e) { toast('Not deleted: ' + e.message, true); }
    });
    return b;
  };
  const clearSd = clearBtn('Delete SD recordings', 'sd', 'every recording on the SD card');
  const clearRam = clearBtn('Delete RAM recordings', 'ram', 'every recording in RAM');
  c.body.append(sw.el,
    h('div', { class: 'row', style: { marginTop: '10px' } }, h('span', { text: 'Save new recordings to' }), where),
    h('div', { class: 'row', style: { marginTop: '8px' } }, h('span', { text: 'Keep in RAM the newest' }), keep),
    h('div', { class: 'row', style: { marginTop: '8px' } }, h('span', { text: 'Keep on the SD card' }), sdKeep, h('span', { text: 'recordings, at most' }), sdMb, h('span', { text: 'MB' }), apply),
    h('div', { style: { marginTop: '12px' } }, kv.el),
    h('div', { class: 'row', style: { marginTop: '10px' } }, clearSd, clearRam),
    h('p', { class: 'card-note', text: 'RAM recordings (tmpfs) are lost on reboot; SD recordings are kept and listed again after a restart. Changing where recordings go does not move existing ones, and each store’s limits only delete its own files (lowering a limit deletes the oldest at once). SD writes happen in the background, so a slow card never holds up decoding; a recording plays as soon as it is listed. If the card is missing, read-only or full, recordings are saved to RAM and the reason shows below. Turning recording off keeps listing calls, without audio.' }));

  function set(d) {
    const r = d.settings.recording;
    const st = d.recording_storage;
    const sd = st.sd || {};
    if (document.activeElement !== sw.input) sw.input.checked = r.enabled;
    setIfIdle(where, r.storage);
    setIfIdle(keep, r.max_count);
    setIfIdle(sdKeep, r.sd_max_count);
    setIfIdle(sdMb, r.sd_max_mb);
    keep.max = d.limits.max_count_max;
    sdKeep.max = d.limits.sd_max_count_max;
    const ram = st.ram || {};
    const sdState = SD_STATE[sd.state] || sd.state || DASH;
    const sdBad = sd.state && !['ok', 'unknown'].includes(sd.state);
    const rows = [
      ['Next recording goes to', st.active === 'sd' ? 'SD card' : 'RAM' + (st.selected === 'sd' ? ' (SD not usable: ' + (sd.ready || sdState) + ')' : ''), st.selected === 'sd' && st.active !== 'sd' ? 'warn' : ''],
      ['In RAM', (ram.count || 0) + ' of ' + r.max_count + ' · ' + bytes(ram.bytes || 0) + (ram.free_bytes != null ? ' · ' + bytes(ram.free_bytes) + ' free' : '')],
      ['On the SD card', (sd.count || 0) + ' of ' + r.sd_max_count + ' · ' + bytes(sd.bytes || 0) + ' of ' + r.sd_max_mb + ' MB'],
      ['SD card', sdState + (sd.detail ? ' — ' + sd.detail : ''), sdBad ? 'bad' : ''],
    ];
    if (sd.free_bytes != null) rows.push(['SD free space', bytes(sd.free_bytes)]);
    if (sd.writes_ok || sd.writes_failed || sd.fallbacks_to_ram) {
      rows.push(['SD writes', sd.writes_ok + ' ok · ' + sd.writes_failed + ' failed · ' + sd.fallbacks_to_ram + ' saved to RAM instead', sd.writes_failed ? 'warn' : '']);
      rows.push(['SD write time', (sd.last_write_ms != null ? sd.last_write_ms + ' ms last' : DASH) + ' · ' + sd.max_write_ms + ' ms slowest']);
    }
    if (sd.queue_jobs) rows.push(['SD queue', sd.queue_jobs + ' waiting · ' + bytes(sd.queue_bytes) + (sd.writing_for_ms ? ' · current write ' + dur(sd.writing_for_ms) : ''), sd.writing_for_ms > 2000 ? 'warn' : '']);
    if (sd.indexed_at_boot) rows.push(['Found at start-up', sd.indexed_at_boot + ' SD recordings']);
    kv.set(rows);
    clearSd.disabled = !sd.count;
    clearRam.disabled = !ram.count;
    clearSd.textContent = 'Delete SD recordings' + (sd.count ? ' (' + sd.count + ')' : '');
    clearRam.textContent = 'Delete RAM recordings' + (ram.count ? ' (' + ram.count + ')' : '');
  }
  return { el: c.el, set };
}

// Change 057: when a call closes (lifecycle, see doc/changes/057).
function callCloseCard(onSaved) {
  const c = card('Call close');
  const grace = num({ min: 0, max: 10000, step: 100 }, 'Close after end of transmission, ms');
  const hang = num({ min: 1000, max: 30000, step: 100 }, 'Close a silent call after, ms');
  const apply = h('button', { class: 'btn small', type: 'button', text: 'Set' });
  apply.addEventListener('click', async () => {
    try {
      await save({ call: { end_grace_ms: parseInt(grace.value, 10), hang_ms: parseInt(hang.value, 10) } });
      toast('Call close timing saved');
      onSaved();
    } catch (e) { toast(e.message, true); }
  });
  c.body.append(
    h('div', { class: 'row' }, h('span', { text: 'Close' }), grace, h('span', { text: 'ms after the end of the transmission' })),
    h('div', { class: 'row', style: { marginTop: '8px' } }, h('span', { text: 'Close a silent call after' }), hang, h('span', { text: 'ms' }), apply),
    h('p', { class: 'card-note', text: 'The end of a transmission is the first valid TDULC after the call’s voice (the frame at which SDRTrunk ends its call). Voice resuming cancels it; a reply granted inside the window becomes the next call at once, and the window keeps the follower on this talkgroup to catch it. The silence timeout is the fallback when no terminator is decoded: voice, an HDU, or a control-channel grant or update for the talkgroup on its channel keeps a call open. The follower is locked to the open call, so shorter windows free it sooner for other talkgroups.' }));
  function set(d) {
    setIfIdle(grace, d.settings.call.end_grace_ms);
    setIfIdle(hang, d.settings.call.hang_ms);
    grace.max = d.limits.end_grace_ms_max;
    hang.min = d.limits.hang_ms_min;
    hang.max = d.limits.hang_ms_max;
  }
  return { el: c.el, set };
}

function encryptedCard() {
  const c = card('Encrypted talkgroups');
  const list = h('div', { class: 'row' });
  const clearAll = h('button', { class: 'btn small danger', type: 'button', text: 'Clear all' });
  c.right.append(clearAll);
  c.body.append(list, h('p', { class: 'card-note', text: 'A talkgroup is added the first time a grant for it is flagged encrypted, and is never followed after that. The list is learned automatically, one per site (a talkgroup number means nothing on another system), and resets on restart; remove a TG here if it was flagged by mistake.' }));
  async function edit(params) {
    try { const r = await api.editEncryptedTgs(params); set(r.tgs); } catch (e) { toast(e.message, true); }
  }
  clearAll.addEventListener('click', () => { if (confirm('Clear the whole encrypted-TG list?')) edit({ clear: 1 }); });
  function set(tgs) {
    list.replaceChildren(...(tgs || []).map(t => h('button', { class: 'chip', type: 'button', title: 'Remove TG ' + t, onclick: () => edit({ remove: t }) }, 'TG ' + t + ' ✕')));
    if (!tgs || !tgs.length) list.append(h('span', { class: 'dim', text: 'None seen.' }));
    clearAll.disabled = !tgs || !tgs.length;
  }
  return { el: c.el, set };
}

// Change 067: the radio clock's source. The radio has no battery-backed
// clock; "site" follows the control channel's time broadcast (works in
// the field without internet), "ntp" internet time, "manual" only the
// button below.
const CLOCK_SOURCES = [
  ['site', 'Control channel (site time)'],
  ['ntp', 'Internet time (NTP)'],
  ['manual', 'Manual (set from a browser)'],
];
const PRECISION = { precise: 'precise', second: 'to the second', minute: 'to the minute (waiting for a minute to roll over)' };

function clockCard() {
  const c = card('Clock');
  const src = h('select', { class: 'input', 'aria-label': 'Radio clock source' },
    ...CLOCK_SOURCES.map(([v, t]) => h('option', { value: v, text: t })));
  src.addEventListener('change', async () => {
    try { await save({ clock: { source: src.value } }); toast('Clock source: ' + src.selectedOptions[0].text); } catch (e) { toast(e.message, true); }
  });
  const note = h('p', { class: 'card-note' });
  const kv = kvTable();
  const sync = h('button', { class: 'btn small', type: 'button', text: 'Set radio clock from this browser' });
  sync.addEventListener('click', async () => {
    try { await api.setTime(Date.now()); toast('Radio clock set'); kick(50); } catch (e) { toast('Failed: ' + e.message, true); }
  });
  const auto = switchInput('Set it automatically when the radio clock is wrong', store.prefs.autoClock, on => setPref('autoClock', on));
  const manual = h('div', null, h('div', { class: 'row', style: { marginTop: '10px' } }, sync), h('div', { style: { marginTop: '8px' } }, auto.el));
  c.body.append(h('div', { class: 'row' }, h('span', { text: 'Radio clock from' }), src), note, kv.el, manual);
  function update(s) {
    const source = (s.site && s.site.clock_source) || 'manual';
    if (document.activeElement !== src) src.value = source;
    const t = s.site && s.site.site_time;
    note.textContent = source === 'site'
      ? "The radio sets its clock from the time the control channel broadcasts, like the site's radios. Call times are then the site's."
      : source === 'ntp'
        ? 'The radio asks internet time servers at start and every hour (needs internet on the radio).'
        : 'The radio clock changes only when set from a browser.';
    manual.hidden = source === 'site';
    const skew = Math.round(store.boardOffsetMs / 1000);
    const rows = [
      ['Radio clock', s.clock_valid ? new Date(s.now_unix_ms).toLocaleString() : 'not set (' + new Date(s.now_unix_ms).toISOString().slice(0, 19) + ')', s.clock_valid ? '' : 'warn'],
    ];
    if (t) {
      rows.push(['Site time', new Date(t.unix_ms).toLocaleDateString() + ' ' + siteClock(t.unix_ms)]);
      if (t.local_offset_min != null) rows.push(['Site time zone (announced)', utcOffset(t.local_offset_min)]);
      rows.push(['Site time quality', (PRECISION[t.precision] || t.precision) + (t.ext_locked ? ', GPS-locked site' : ', site clock not GPS-locked')]);
      rows.push(['Radio vs site', dur(Math.abs(t.board_offset_ms)) + (t.board_offset_ms > 0 ? ' behind' : t.board_offset_ms < 0 ? ' ahead' : ''),
        Math.abs(t.board_offset_ms) > 2000 && source === 'site' ? 'warn' : '']);
    } else {
      rows.push(['Site time', 'no time broadcast decoded yet']);
    }
    rows.push(['Offset to this browser', s.clock_valid ? skew + ' s' : DASH, Math.abs(skew) > 120 && source !== 'site' ? 'warn' : '']);
    kv.set(rows);
  }
  return { el: c.el, update };
}

// Change 069: the site's profiles. The cards below it that belong to
// the profile or the site say so in their titles.
function profilesCard(onChange) {
  const c = card('Profiles');
  const picker = profilePicker({ manage: true, onChange });
  c.body.append(picker.el,
    h('p', { class: 'card-note', text: 'A profile holds the talkgroup groups, the speakers, the monitor list and the ignored talkgroups. Each site has its own profiles and its own talkgroup and radio names; switching site on the Radio page loads the profile last used there. Everything else here is for the whole radio.' }));
  return { el: c.el, set: picker.set };
}

function displayCard() {
  const c = card('This browser');
  const theme = h('select', { class: 'input', 'aria-label': 'Theme' },
    h('option', { value: 'auto', text: 'Theme: system' }), h('option', { value: 'dark', text: 'Theme: dark' }), h('option', { value: 'light', text: 'Theme: light' }));
  theme.value = store.prefs.theme;
  theme.addEventListener('change', () => setPref('theme', theme.value));
  const live = switchInput('Live push (events socket) for instant call updates', store.prefs.liveEvents, on => setPref('liveEvents', on));
  const nf = switchInput('Show encrypted / not-followed grants in Recent calls', store.prefs.showNotFollowed, on => setPref('showNotFollowed', on));
  c.body.append(theme, h('div', { style: { marginTop: '10px' } }, live.el), h('div', { style: { marginTop: '8px' } }, nf.el));
  return { el: c.el };
}

function aboutCard() {
  const c = card('About');
  const kv = kvTable();
  c.body.append(kv.el);
  function set(d, s) {
    kv.set([
      ['Build', s ? s.build : DASH],
      ['Settings file', d.file || 'none (not persisted)'],
      ['Loaded from', d.load_note],
      ['Last save error', d.last_save_error || 'none', d.last_save_error ? 'bad' : ''],
    ]);
  }
  return { el: c.el, set };
}

export function mount(host) {
  let data = null;
  const load = async () => {
    try {
      data = await api.settings();
      rec.set(data);
      callClose.set(data);
      tgAliases.set(data.settings.tg_aliases);
      unitAliases.set(data.settings.unit_aliases);
      monitor.set(data.settings.monitor_tgs, data.settings.tg_aliases);
      ignore.set(data.settings.ignore_tgs, data.settings.tg_aliases);
      groups.set(data);
      profiles.set(data);
      markScope(data);
      enc.set(data.encrypted_tgs);
      about.set(data, store.state);
    } catch (e) {
      toast('Settings unavailable: ' + e.message, true);
    }
  };
  const rec = recordingCard(load);
  const callClose = callCloseCard(load);
  const tgAliases = aliasEditor({
    title: 'Talkgroup names', idLabel: 'TG', maxId: 16777215,
    onSave: async map => { await save({ tg_aliases: map }); load(); },
  });
  const unitAliases = aliasEditor({
    title: 'Radio names', idLabel: 'Radio ID', maxId: 16777215,
    note: 'Names for source radio IDs (dispatch consoles, units).',
    onSave: async map => { await save({ unit_aliases: map }); load(); },
  });
  const monitor = monitorPicker({ onSave: async list => { await save({ monitor_tgs: list }); load(); } });
  const ignore = ignoreList({ onSave: async list => { await save({ ignore_tgs: list }); load(); } });
  const enc = encryptedCard();
  const clock = clockCard();
  const display = displayCard();
  const groups = tgGroupsEditor({ onSave: async patch => { await save(patch); load(); } });
  const about = aboutCard();
  const profiles = profilesCard(load);
  // Change 069: which cards follow the profile / the site.
  const scoped = [[groups.el, 'profile'], [monitor.el, 'profile'], [ignore.el, 'profile'],
    [tgAliases.el, 'site'], [unitAliases.el, 'site'], [enc.el, 'site']].map(([el, kind]) => {
    const tag = h('span', { class: 'dim', style: { fontSize: '0.84em', marginLeft: '8px', fontWeight: 'normal' } });
    const title = el.querySelector('.card-head h2');
    if (title) title.append(tag);
    return [tag, kind];
  });
  function markScope(d) {
    const p = d.profiles || {};
    for (const [tag, kind] of scoped) {
      tag.textContent = kind === 'profile' ? (p.active ? '· ' + p.active : '') : (p.site_label || p.site ? '· ' + (p.site_label || p.site) : '');
    }
  }
  host.append(
    h('div', { class: 'grid-2' },
      h('div', { class: 'stack' }, rec.el, callClose.el, tgAliases.el, unitAliases.el),
      h('div', { class: 'stack' }, profiles.el, groups.el, monitor.el, ignore.el, enc.el, clock.el, display.el, about.el)),
  );
  load();
  let lastRev = null;
  return {
    update(kind, st) {
      if (kind !== 'state' || !st.state) return;
      clock.update(st.state);
      if (lastRev !== null && st.state.settings_rev !== lastRev) load();
      lastRev = st.state.settings_rev;
      if (data) about.set(data, st.state);
    },
    unmount() {},
  };
}
