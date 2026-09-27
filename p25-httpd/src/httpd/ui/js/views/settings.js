// "Settings": recording on/off + retention, aliases, monitor list,
// encrypted lockout, clock, display. Radio-side settings are persisted
// on the board (/mnt/jffs2/p25-ui-settings.json); display settings
// live in this browser only.

import { h, card, toast, switchInput } from '../dom.js';
import { api } from '../api.js';
import { store, setPref, kick } from '../store.js';
import { bytes, DASH } from '../format.js';
import { kvTable } from '../components/kv_table.js';
import { aliasEditor } from '../components/alias_editor.js';
import { monitorPicker } from '../components/monitor_picker.js';

async function save(patch) {
  const r = await api.putSettings(patch);
  if (!r.persisted) toast('Applied, but NOT saved to flash: ' + (r.save_error || 'no settings file'), true);
  kick(50);
  return r;
}

function recordingCard(onSaved) {
  const c = card('Recording');
  const kv = kvTable();
  const sw = switchInput('Save a recording of every followed call', true, async on => {
    try { await save({ recording: { enabled: on } }); onSaved(); } catch (e) { sw.input.checked = !on; toast(e.message, true); }
  });
  const keep = h('input', { class: 'input num', type: 'number', min: 1, max: 500, 'aria-label': 'Recordings to keep' });
  const apply = h('button', { class: 'btn small', type: 'button', text: 'Set' });
  apply.addEventListener('click', async () => {
    const n = parseInt(keep.value, 10);
    try {
      const r = await save({ recording: { max_count: n } });
      toast('Keeping ' + n + ' recordings' + (r.evicted ? ' (' + r.evicted + ' deleted)' : ''));
      onSaved();
    } catch (e) { toast(e.message, true); }
  });
  c.body.append(sw.el,
    h('div', { class: 'row', style: { marginTop: '10px' } }, h('span', { text: 'Keep the newest' }), keep, h('span', { text: 'recordings' }), apply),
    h('div', { style: { marginTop: '12px' } }, kv.el),
    h('p', { class: 'card-note', text: 'Recordings are WAV files in RAM (tmpfs) and are lost on reboot; lowering the number deletes the oldest at once. Turning recording off keeps listing calls, without audio.' }));

  function set(d) {
    const r = d.settings.recording;
    if (document.activeElement !== sw.input) sw.input.checked = r.enabled;
    if (document.activeElement !== keep) keep.value = r.max_count;
    keep.max = d.limits.max_count_max;
    const st = d.recording_storage;
    kv.set([
      ['Stored now', st.count + ' of ' + r.max_count],
      ['Location', st.dir + (st.tmpfs ? ' (RAM)' : '')],
      ['Free space', st.free_bytes != null ? bytes(st.free_bytes) : DASH],
    ]);
  }
  return { el: c.el, set };
}

function encryptedCard() {
  const c = card('Encrypted talkgroups');
  const list = h('div', { class: 'row' });
  const clearAll = h('button', { class: 'btn small danger', type: 'button', text: 'Clear all' });
  c.right.append(clearAll);
  c.body.append(list, h('p', { class: 'card-note', text: 'A talkgroup is added the first time a grant for it is flagged encrypted, and is never followed after that. The list is learned automatically and resets on restart; remove a TG here if it was flagged by mistake.' }));
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

function clockCard() {
  const c = card('Clock');
  const kv = kvTable();
  const sync = h('button', { class: 'btn small', type: 'button', text: 'Set radio clock from this browser' });
  sync.addEventListener('click', async () => {
    try { await api.setTime(Date.now()); toast('Radio clock set'); kick(50); } catch (e) { toast('Failed: ' + e.message, true); }
  });
  const auto = switchInput('Set it automatically when the radio clock is wrong', store.prefs.autoClock, on => setPref('autoClock', on));
  c.body.append(kv.el, h('div', { class: 'row', style: { marginTop: '10px' } }, sync), h('div', { style: { marginTop: '8px' } }, auto.el));
  function update(s) {
    const skew = Math.round(store.boardOffsetMs / 1000);
    kv.set([
      ['Radio clock', s.clock_valid ? new Date(s.now_unix_ms).toLocaleString() : 'not set (' + new Date(s.now_unix_ms).toISOString().slice(0, 19) + ')', s.clock_valid ? '' : 'warn'],
      ['Offset to this browser', s.clock_valid ? skew + ' s' : DASH, Math.abs(skew) > 120 ? 'warn' : ''],
    ]);
  }
  return { el: c.el, update };
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
  c.body.append(kv.el, h('p', { class: 'card-note' }, 'Previous dashboard: ', h('a', { href: '/legacy' }, '/legacy'), '.'));
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
      tgAliases.set(data.settings.tg_aliases);
      unitAliases.set(data.settings.unit_aliases);
      monitor.set(data.settings.monitor_tgs, data.settings.tg_aliases);
      enc.set(data.encrypted_tgs);
      about.set(data, store.state);
    } catch (e) {
      toast('Settings unavailable: ' + e.message, true);
    }
  };
  const rec = recordingCard(load);
  const tgAliases = aliasEditor({
    title: 'Talkgroup names', idLabel: 'TG', maxId: 65535,
    onSave: async map => { await save({ tg_aliases: map }); load(); },
  });
  const unitAliases = aliasEditor({
    title: 'Radio names', idLabel: 'Radio ID', maxId: 16777215,
    note: 'Names for source radio IDs (dispatch consoles, units).',
    onSave: async map => { await save({ unit_aliases: map }); load(); },
  });
  const monitor = monitorPicker({ onSave: async list => { await save({ monitor_tgs: list }); load(); } });
  const enc = encryptedCard();
  const clock = clockCard();
  const display = displayCard();
  const about = aboutCard();
  host.append(
    h('div', { class: 'grid-2' },
      h('div', { class: 'stack' }, rec.el, tgAliases.el, unitAliases.el),
      h('div', { class: 'stack' }, monitor.el, enc.el, clock.el, display.el, about.el)),
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
