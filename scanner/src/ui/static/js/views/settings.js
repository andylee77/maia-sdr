// Settings, as two columns of cards: the receiver's gain, the radio's other settings (window
// presets, call timings, the history's limits), the clock and the configuration as a whole
// (export, import, factory reset); recording, the crystal and packet data. A setting is a row:
// what it is on the left, its control on the right.

import { h, card, toast, switchInput } from '../dom.js';
import { api } from '../api.js';
import { store, subscribe, want, boardNow } from '../store.js';
import { ago, bytes, num } from '../format.js';
import { crystalSource, packetDataSetting } from '../protocols.js';

const MODES = [['slow_attack', 'AGC, slow'], ['fast_attack', 'AGC, fast'], ['hybrid', 'AGC, hybrid'], ['manual', 'Manual']];

// A setting's row: its name, a note under it, and its control (one node or several).
function row(label, control, note) {
  return h('div', { class: 'set-row' },
    h('div', { class: 'set-what' }, h('div', { text: label }), note ? h('div', { class: 'sub', text: note }) : null),
    h('div', { class: 'set-ctl' }, ...[].concat(control)));
}

const unit = text => h('span', { class: 'dim', text });
const count = (value, min, max, label) => h('input', { class: 'input num', type: 'number', min, max, step: 1, value, 'aria-label': label });

// The number typed in `input`. An empty box or other text is refused, not read as 0.
function typed(input, what, { whole = true } = {}) {
  const text = String(input.value).trim();
  const n = Number(text);
  if (!text || !Number.isFinite(n) || (whole && !Number.isInteger(n))) throw new Error(`${what}: ${whole ? 'a whole number' : 'a number'}`);
  return n;
}

// A card's last line: a note on the left, its buttons on the right.
function foot(note, ...buttons) {
  return h('div', { class: 'set-foot' }, h('p', { class: 'card-note', text: note || '' }), h('div', { class: 'set-ctl' }, ...buttons));
}

function gainCard(radio) {
  const c = card('Receiver');
  const mode = h('select', { class: 'input', 'aria-label': 'Gain mode' }, ...MODES.map(([v, t]) => h('option', { value: v, text: t })));
  mode.value = radio.config.gain.mode;
  const db = h('input', { class: 'input num', type: 'number', min: -3, max: 76, step: 1, value: radio.config.gain.manual_db ?? 60, 'aria-label': 'Manual gain, dB' });
  const apply = h('button', { class: 'btn', type: 'button', text: 'Apply' });
  const sync = () => { db.disabled = mode.value !== 'manual'; };
  mode.addEventListener('change', sync);
  sync();
  apply.addEventListener('click', async () => {
    try {
      await api.setGain(mode.value, mode.value === 'manual' ? typed(db, 'Manual gain', { whole: false }) : null);
      toast('Gain applied');
    } catch (e) {
      toast(e.message, true);
    }
  });
  c.body.append(row('Gain', [mode, db, unit('dB'), apply], 'Applied at once.'));
  return c.el;
}

function recordingCard(radio, recs) {
  const c = card('Recording');
  const r = radio.config.recording;
  const on = switchInput('', r.enabled, () => {});
  on.input.setAttribute('aria-label', 'Record followed calls');
  const where = h('select', { class: 'input', 'aria-label': 'New recordings go to' }, h('option', { value: 'sd', text: 'SD card' }), h('option', { value: 'ram', text: 'RAM (lost on reboot)' }));
  where.value = r.storage;
  const ramN = count(r.ram_max_count, 1, 10000, 'RAM keeps, recordings');
  const sdDays = count(r.sd_keep_days, 1, 3650, 'SD card keeps, days');
  const lockedDays = count(r.locked_keep_days, 1, 3650, 'Locked ones, days');
  const sdMb = count(r.sd_max_mb, 16, 1048576, 'SD card, at most MB');
  const perFolder = count(r.folder_max_count, 100, 6000, 'Recordings a folder');
  const save = h('button', { class: 'btn primary', type: 'button', text: 'Save' });
  save.addEventListener('click', async () => {
    try {
      const next = {
        enabled: on.input.checked, every_call: r.every_call, storage: where.value,
        ram_max_count: typed(ramN, 'RAM keeps'), sd_keep_days: typed(sdDays, 'The card keeps a recording'), locked_keep_days: typed(lockedDays, 'A locked recording'),
        sd_max_mb: typed(sdMb, 'The card holds at most'), folder_max_count: typed(perFolder, "A day's folder holds at most"),
      };
      // A lowered limit deletes what is past it as it is saved.
      const lowered = ['ram_max_count', 'sd_keep_days', 'locked_keep_days', 'sd_max_mb'].some(k => next[k] < r[k]);
      if (lowered && !confirm('A lower limit deletes the recordings past it at once. This cannot be undone. Save?')) return;
      const res = await api.setRecording(next);
      Object.assign(r, res.recording);
      toast(res.deleted ? `Saved; ${res.deleted} oldest recording(s) deleted` : 'Saved');
    } catch (e) {
      toast(e.message, true);
    }
  });
  const clearBtn = (label, which, what) => h('button', {
    class: 'btn small danger', type: 'button', text: label,
    onclick: async () => {
      if (!confirm(`Delete ${what}? This cannot be undone.`)) return;
      try {
        const res = await api.clearRecordings(which);
        toast(`${res.deleted} recording(s) deleted`);
      } catch (e) {
        toast(e.message, true);
      }
    },
  });
  const st = recs.storage;
  const sd = st.sd_status;
  const free = sd.free_bytes !== null && sd.free_bytes !== undefined ? `, ${bytes(sd.free_bytes)} free` : '';
  c.body.append(
    row('Record followed calls', on.el),
    row('New recordings go to', where),
    row('RAM keeps', [ramN, unit('recordings')]),
    row('The card keeps a recording', [sdDays, unit('days')]),
    row('A locked recording', [lockedDays, unit('days')], 'Calls with alert tones are locked; any recording on the card can be locked from Now or Activity.'),
    row('The card holds at most', [sdMb, unit('MB')], 'Past it, the oldest unlocked go first.'),
    row('A day\'s folder holds at most', [perFolder, unit('recordings')]),
    foot(`SD card (${sd.state.replace('_', ' ')}${free}): ${num(st.sd.count)} recordings, ${bytes(st.sd.bytes)}, ${num(st.locked.count)} locked. RAM: ${num(st.ram.count)}, ${bytes(st.ram.bytes)}.${sd.state === 'failing' && sd.detail ? ` The last write failed: ${sd.detail}` : ''}`, save),
    foot('', clearBtn('Delete SD recordings', 'sd', 'every recording on the SD card'), clearBtn('Delete RAM recordings', 'ram', 'every recording in RAM')));
  return c.el;
}

function radioCard(radio) {
  const c = card('Radio');
  const r = radio.config;
  const presets = radio.presets.map(name => {
    const box = h('input', { type: 'checkbox', value: name });
    box.checked = r.presets_allowed.includes(name);
    return { name, box, el: h('label', { class: 'chip' }, box, h('span', { text: name })) };
  });
  const hang = count(r.calls.hang_ms, 500, 30000, 'Hang time, ms');
  const grace = count(r.calls.end_grace_ms, 0, 10000, 'End grace, ms');
  const days = count(r.history.retention_days, 1, 3650, 'History, days');
  const mb = count(r.history.sd_max_mb, 16, 32768, 'History, MB');
  const save = h('button', { class: 'btn primary', type: 'button', text: 'Save' });
  save.addEventListener('click', async () => {
    try {
      const next = {
        presets_allowed: presets.filter(p => p.box.checked).map(p => p.name),
        traffic_chains: r.traffic_chains,
        calls: { hang_ms: typed(hang, 'A call closes after'), end_grace_ms: typed(grace, 'A call closes after its end') },
        history: { retention_days: typed(days, 'The history is kept'), sd_max_mb: typed(mb, 'The history holds at most') },
      };
      // A lowered limit drops the calls past it as it is saved.
      const lowered = next.history.retention_days < r.history.retention_days || next.history.sd_max_mb < r.history.sd_max_mb;
      if (lowered && !confirm('A lower limit deletes the history past it at once. This cannot be undone. Save?')) return;
      const kept = await api.saveRadioSettings(next);
      r.history = kept.history;
      toast('Saved; the window presets and call timings apply at the next site switch');
    } catch (e) {
      toast(e.message, true);
    }
  });
  c.body.append(
    h('div', { class: 'set-row wide' },
      h('div', { class: 'set-what' }, h('div', { text: 'Window presets the planner may use' })),
      h('div', { class: 'chips' }, ...presets.map(p => p.el))),
    row('A call closes after', [hang, unit('ms')], 'With no sign of life.'),
    row('…or after its end', [grace, unit('ms')]),
    row('The history is kept', [days, unit('days')]),
    row('The history holds at most', [mb, unit('MB')]),
    foot('The window presets and the call timings apply at the next site switch.', save));
  return c.el;
}

function clockCard(radio) {
  const c = card('Clock');
  const source = h('select', { class: 'input', 'aria-label': 'Clock' },
    ...[['site', 'The control channel'], ['ntp', 'The internet'], ['manual', 'By hand']].map(([v, t]) => h('option', { value: v, text: t })));
  let kept = radio.config.clock.source;
  source.value = kept;
  source.addEventListener('change', async () => {
    try {
      await api.setClockSource(source.value);
      kept = source.value;
      toast('Clock source saved');
    } catch (e) {
      source.value = kept;
      toast(e.message, true);
    }
  });
  const setNow = h('button', { class: 'btn', type: 'button', text: 'Set it now' });
  setNow.addEventListener('click', async () => {
    try {
      await api.setTime(Date.now());
      toast('Clock set');
    } catch (e) {
      toast(e.message, true);
    }
  });
  c.body.append(
    row('The clock comes from', source, 'Saved as it is picked.'),
    row('Set it to this browser\'s time', setNow));
  return c.el;
}

// Packet data followed or not; the radio takes a change at once.
function packetDataCard(radio) {
  const c = card('Packet data');
  const follow = switchInput('', radio.config.packet_data.follow, async on => {
    try {
      const kept = await api.setPacketData(on);
      toast(kept.follow ? 'Data traffic is followed' : 'Data traffic is not followed');
    } catch (e) {
      follow.input.checked = !on;
      toast(e.message, true);
    }
  });
  follow.input.setAttribute('aria-label', packetDataSetting.label);
  c.body.append(row(packetDataSetting.label, follow.el, packetDataSetting.note));
  return c.el;
}

const ppm = v => `${v >= 0 ? '+' : ''}${v.toFixed(4)} ppm`;
const signedHz = v => `${v >= 0 ? '+' : ''}${Math.round(v)} Hz`;

// The crystal correction, pushed by the radio while Settings is open.
function crystalCard(initial) {
  const c = card('Crystal');
  const lines = h('dl', { class: 'kv' });
  const anchor = count(initial.anchor_hz, 0, 1000, 'Anchor, Hz');
  // True once the radio holds it.
  const save = async (on) => {
    try {
      show(await api.setCrystal({ tracking: on, anchor_hz: typed(anchor, 'The anchor') }));
      toast('Crystal tracking saved');
      return true;
    } catch (e) {
      toast(e.message, true);
      return false;
    }
  };
  const tracking = switchInput('', initial.tracking, async on => {
    if (!(await save(on))) tracking.input.checked = !on;
  });
  tracking.input.setAttribute('aria-label', 'Follow its drift');
  const saveAnchor = h('button', { class: 'btn', type: 'button', text: 'Save' });
  saveAnchor.addEventListener('click', () => save(tracking.input.checked));
  const calibrate = h('button', { class: 'btn', type: 'button', text: 'Calibrate now' });
  calibrate.addEventListener('click', async () => {
    calibrate.disabled = true;
    try {
      const cal = await api.calibrateCrystal();
      toast(`Calibrated: ${ppm(cal.ppm)}`);
      show(await api.crystal());
    } catch (e) {
      toast(e.message, true);
    } finally {
      calibrate.disabled = false;
    }
  });
  function show(s) {
    const cal = s.calibration;
    const rows = [
      ['Correction', `${ppm(s.ppm)} (LO ${signedHz(s.lo_shift_hz)})`],
      ['Measured from', s.source ? crystalSource(s.source) : 'nothing yet: the live site is not decoded'],
      ['Calibrated', cal
        ? `${ppm(cal.ppm)} ${ago(boardNow() - cal.at_unix_ms)} (spectrum ${signedHz(cal.spectrum_offset_hz)}, residual ${cal.residual_hz === null ? 'not measured' : signedHz(cal.residual_hz)})`
        : (s.calibrating ? 'now' : 'not since start')],
      ['Tracker', s.estimate_ppm === null ? `${num(s.samples)} samples of 60` : `${ppm(s.estimate_ppm)} from ${num(s.samples)} samples`],
      ...(s.last_decision ? [['Last', s.last_decision]] : []),
    ];
    lines.replaceChildren(...rows.flatMap(([k, v]) => [h('dt', { text: k }), h('dd', { text: v })]));
  }
  show(initial);
  c.body.append(
    lines,
    row('Follow its drift', tracking.el),
    row('…within', [anchor, unit('Hz of the calibration'), saveAnchor]),
    foot('The crystal is measured on the live control channel: once the site is decoded after start, and then followed as it warms and cools.', calibrate));
  const stop = want({ crystal: true }, m => show(m.crystal));
  return { el: c.el, stop };
}

// After an import or a reset the scanner restarts: reload once it is back (the store's socket
// dropped and came back, or brings a status of less uptime than before).
function reloadAfterRestart() {
  const before = store.status ? store.status.uptime_s : null;
  let down = false;
  subscribe(s => {
    if (!s.connected) down = true;
    else if (s.status && (down || (before !== null && s.status.uptime_s < before))) location.reload();
  });
}

function configCard() {
  const c = card('Configuration');
  const exportLink = h('a', { class: 'btn', href: '/api/v1/config?download=true', download: 'scanner-config.json', text: 'Export' });
  const file = h('input', { class: 'input', type: 'file', accept: '.json,application/json', 'aria-label': 'Configuration file' });
  const importBtn = h('button', { class: 'btn', type: 'button', text: 'Import' });
  importBtn.addEventListener('click', async () => {
    const f = file.files && file.files[0];
    if (!f) {
      toast('Choose an exported configuration file first', true);
      return;
    }
    let doc;
    try {
      doc = JSON.parse(await f.text());
    } catch (e) {
      toast(`${f.name} is not JSON`, true);
      return;
    }
    if (!confirm(`Replace the radio settings, systems, sites and aliases with ${f.name}? The radio restarts.`)) return;
    importBtn.disabled = reset.disabled = true;
    try {
      const r = await api.importConfig(doc);
      toast(`Imported ${r.systems} systems, ${r.sites} sites, ${r.aliases} aliases; restarting`);
      reloadAfterRestart();
    } catch (e) {
      toast(e.message, true);
      importBtn.disabled = reset.disabled = false;
    }
  });
  const reset = h('button', { class: 'btn danger', type: 'button', text: 'Factory reset' });
  reset.addEventListener('click', async () => {
    if (!confirm('Factory reset: delete every system (with its aliases), site, recording, the call history and the TV channel list, and restore the default settings? The crystal calibration stays. This cannot be undone.')) return;
    importBtn.disabled = reset.disabled = true;
    try {
      const r = await api.factoryReset();
      toast(`Reset: ${r.sites} sites, ${r.recordings} recordings, ${r.calls} calls removed; restarting`);
      reloadAfterRestart();
    } catch (e) {
      toast(e.message, true);
      importBtn.disabled = reset.disabled = false;
    }
  });
  c.body.append(
    row('Export', exportLink, 'The radio settings and the systems with their aliases and sites, as one file. What the radio learned on the air and the crystal calibration stay with the board.'),
    row('Import', [file, importBtn], 'Replaces the settings, systems, sites and aliases with a file\'s; the radio restarts.'),
    row('Factory reset', reset, 'Deletes every system, site, recording, the call history and the TV channel list, and restores the default settings. The crystal calibration stays.'));
  return c.el;
}

export function mount(el) {
  const left = h('div', { class: 'stack' });
  const right = h('div', { class: 'stack' });
  el.append(h('div', { class: 'settings' }, left, right));
  let crystal = null;
  // The page was left before the radio answered: nothing is built, so nothing is left asking.
  let gone = false;
  Promise.all([api.radio(), api.crystal(), api.recordings(0)])
    .then(([radio, crystalStatus, recs]) => {
      if (gone) return;
      crystal = crystalCard(crystalStatus);
      left.append(gainCard(radio), radioCard(radio), clockCard(radio), configCard());
      right.append(recordingCard(radio, recs), crystal.el, packetDataCard(radio));
    })
    .catch(e => toast(e.message, true));
  return {
    update() {},
    unmount() {
      gone = true;
      if (crystal) crystal.stop();
    },
  };
}
