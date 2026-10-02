// Settings: the receiver gain, the radio's other settings, the crystal, recording, profiles, each
// system's talkgroup and radio names, and the configuration as a whole (export, import, factory
// reset).

import { h, card, clear, toast, switchInput } from '../dom.js';
import { api } from '../api.js';
import { ago, bytes, num } from '../format.js';
import { crystalSource } from '../protocols.js';
import { profilesCard } from './profiles.js';

const MODES = [['slow_attack', 'AGC, slow'], ['fast_attack', 'AGC, fast'], ['hybrid', 'AGC, hybrid'], ['manual', 'Manual']];

function gainCard(radio) {
  const c = card('Receiver gain');
  const mode = h('select', { class: 'input' }, ...MODES.map(([v, t]) => h('option', { value: v, text: t })));
  mode.value = radio.config.gain.mode;
  const db = h('input', { class: 'input', type: 'number', min: -3, max: 76, step: 1, value: radio.config.gain.manual_db ?? 60 });
  const apply = h('button', { class: 'btn', type: 'button', text: 'Apply' });
  const sync = () => { db.disabled = mode.value !== 'manual'; };
  mode.addEventListener('change', sync);
  sync();
  apply.addEventListener('click', async () => {
    try {
      await api.setGain(mode.value, mode.value === 'manual' ? Number(db.value) : null);
      toast('Gain applied');
    } catch (e) {
      toast(e.message, true);
    }
  });
  c.body.append(h('div', { class: 'row' }, mode, db, h('span', { class: 'dim', text: 'dB' }), apply));
  return c.el;
}

function recordingCard(radio, recs) {
  const c = card('Recording');
  const r = radio.config.recording;
  const on = switchInput('Record followed calls', r.enabled, () => {});
  const where = h('select', { class: 'input' }, h('option', { value: 'sd', text: 'SD card' }), h('option', { value: 'ram', text: 'RAM (lost on reboot)' }));
  where.value = r.storage;
  const count = (value, max) => h('input', { class: 'input num', type: 'number', min: 1, max, step: 1, value });
  const ramN = count(r.ram_max_count, 10000);
  const sdN = count(r.sd_max_count, 100000);
  const sdMb = h('input', { class: 'input num', type: 'number', min: 16, max: 32768, step: 1, value: r.sd_max_mb });
  const save = h('button', { class: 'btn primary', type: 'button', text: 'Save' });
  save.addEventListener('click', async () => {
    try {
      const res = await api.setRecording({
        enabled: on.input.checked, storage: where.value,
        ram_max_count: Number(ramN.value), sd_max_count: Number(sdN.value), sd_max_mb: Number(sdMb.value),
      });
      toast(res.deleted ? `Saved; ${res.deleted} oldest recording(s) deleted` : 'Saved');
    } catch (e) {
      toast(e.message, true);
    }
  });
  const clearBtn = (label, store, what) => h('button', {
    class: 'btn small danger', type: 'button', text: label,
    onclick: async () => {
      if (!confirm(`Delete ${what}? This cannot be undone.`)) return;
      try {
        const res = await api.clearRecordings(store);
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
    h('div', { class: 'row' }, on.el),
    h('div', { class: 'row' }, h('span', { text: 'New recordings go to' }), h('div', { class: 'spacer' }), where),
    h('div', { class: 'row' }, h('span', { text: 'RAM keeps' }), h('div', { class: 'spacer' }), ramN, h('span', { class: 'dim', text: 'recordings' })),
    h('div', { class: 'row' }, h('span', { text: 'SD card keeps' }), h('div', { class: 'spacer' }), sdN,
      h('span', { class: 'dim', text: 'recordings, at most' }), sdMb, h('span', { class: 'dim', text: 'MB' })),
    h('div', { class: 'row end' }, save),
    h('p', { class: 'card-note', text: `SD card (${sd.state.replace('_', ' ')}${free}): ${num(st.sd.count)} recordings, ${bytes(st.sd.bytes)}. RAM: ${num(st.ram.count)}, ${bytes(st.ram.bytes)}.` }),
    h('div', { class: 'row' }, clearBtn('Delete SD recordings', 'sd', 'every recording on the SD card'), clearBtn('Delete RAM recordings', 'ram', 'every recording in RAM')));
  return c.el;
}

function radioCard(radio) {
  const c = card('Radio');
  const r = radio.config;
  const presets = radio.presets.map(name => {
    const box = h('input', { type: 'checkbox', value: name });
    box.checked = r.presets_allowed.includes(name);
    return { name, box, el: h('label', { class: 'row', style: { gap: '4px' } }, box, h('span', { text: name })) };
  });
  const num_ = (value, min, max, label) => h('input', { class: 'input num', type: 'number', min, max, step: 1, value, 'aria-label': label });
  const hang = num_(r.calls.hang_ms, 500, 30000, 'Hang time, ms');
  const grace = num_(r.calls.end_grace_ms, 0, 10000, 'End grace, ms');
  const days = num_(r.history.retention_days, 1, 3650, 'History, days');
  const mb = num_(r.history.sd_max_mb, 16, 32768, 'History, MB');
  const save = h('button', { class: 'btn primary', type: 'button', text: 'Save' });
  save.addEventListener('click', async () => {
    try {
      await api.saveRadioSettings({
        presets_allowed: presets.filter(p => p.box.checked).map(p => p.name),
        traffic_chains: r.traffic_chains,
        calls: { hang_ms: Number(hang.value), end_grace_ms: Number(grace.value) },
        history: { retention_days: Number(days.value), sd_max_mb: Number(mb.value) },
      });
      toast('Saved; the window presets and call timings apply at the next site switch');
    } catch (e) {
      toast(e.message, true);
    }
  });
  const clockSource = h('select', { class: 'input', 'aria-label': 'Clock' },
    ...[['site', 'The control channel'], ['ntp', 'The internet'], ['manual', 'By hand']].map(([v, t]) => h('option', { value: v, text: t })));
  clockSource.value = r.clock.source;
  clockSource.addEventListener('change', async () => {
    try {
      await api.setClockSource(clockSource.value);
      toast('Clock source saved');
    } catch (e) {
      toast(e.message, true);
    }
  });
  const setNow = h('button', { class: 'btn small', type: 'button', text: 'Set it to this browser\'s time' });
  setNow.addEventListener('click', async () => {
    try {
      await api.setTime(Date.now());
      toast('Clock set');
    } catch (e) {
      toast(e.message, true);
    }
  });
  c.body.append(
    h('div', { class: 'row' }, h('span', { text: 'The clock comes from' }), clockSource, setNow),
    h('div', { class: 'row' }, h('span', { text: 'Window presets the planner may use' }), ...presets.map(p => p.el)),
    h('div', { class: 'row' }, h('span', { text: 'A call closes after' }), hang, h('span', { class: 'dim', text: 'ms with no sign of life, or' }), grace,
      h('span', { class: 'dim', text: 'ms after its end' })),
    h('div', { class: 'row' }, h('span', { text: 'Keep the history' }), days, h('span', { class: 'dim', text: 'days, at most' }), mb, h('span', { class: 'dim', text: 'MB' })),
    h('div', { class: 'row end' }, save));
  return c.el;
}

const ppm = v => `${v >= 0 ? '+' : ''}${v.toFixed(4)} ppm`;
const signedHz = v => `${v >= 0 ? '+' : ''}${Math.round(v)} Hz`;

// The crystal correction, refreshed every few seconds while Settings is open.
function crystalCard(initial) {
  const c = card('Crystal');
  const lines = h('div', { class: 'stack' });
  const anchor = h('input', { class: 'input num', type: 'number', min: 0, max: 1000, step: 1, value: initial.anchor_hz, 'aria-label': 'Anchor, Hz' });
  const save = async (on) => {
    try {
      show(await api.setCrystal({ tracking: on, anchor_hz: Number(anchor.value) }));
      toast('Crystal tracking saved');
    } catch (e) {
      toast(e.message, true);
    }
  };
  const tracking = switchInput('Follow its drift', initial.tracking, on => save(on));
  const saveAnchor = h('button', { class: 'btn small', type: 'button', text: 'Save' });
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
    const row = (k, v) => h('div', { class: 'row' }, h('span', { class: 'dim', text: k }), h('span', { text: v }));
    const cal = s.calibration;
    clear(lines);
    lines.append(
      row('Correction', `${ppm(s.ppm)} (LO ${signedHz(s.lo_shift_hz)})`),
      row('Measured from', s.source ? crystalSource(s.source) : 'nothing yet: the live site is not decoded'),
      row('Calibrated', cal
        ? `${ppm(cal.ppm)} ${ago(Date.now() - cal.at_unix_ms)} (spectrum ${signedHz(cal.spectrum_offset_hz)}, residual ${cal.residual_hz === null ? 'not measured' : signedHz(cal.residual_hz)})`
        : (s.calibrating ? 'now' : 'not since start')),
      row('Tracker', s.estimate_ppm === null ? `${num(s.samples)} samples of 60` : `${ppm(s.estimate_ppm)} from ${num(s.samples)} samples`),
      ...(s.last_decision ? [row('Last', s.last_decision)] : []));
  }
  show(initial);
  c.body.append(
    lines,
    h('div', { class: 'row' }, tracking.el, h('span', { class: 'dim', text: 'within' }), anchor, h('span', { class: 'dim', text: 'Hz of the calibration' }), saveAnchor),
    h('div', { class: 'row end' }, calibrate),
    h('p', { class: 'card-note', text: 'The crystal is measured on the live control channel: once the site is decoded after start, and then followed as it warms and cools.' }));
  const timer = setInterval(() => api.crystal().then(show).catch(() => {}), 5000);
  return { el: c.el, stop: () => clearInterval(timer) };
}

// "300 = Fire Dispatch" lines <-> {300: "Fire Dispatch"}.
function namesText(map) {
  return Object.entries(map || {}).map(([k, v]) => `${k} = ${v}`).join('\n');
}

function parseNames(text, what) {
  const out = {};
  for (const line of text.split('\n').map(l => l.trim()).filter(Boolean)) {
    const m = line.match(/^(\d+)\s*[=:,]\s*(.+)$/);
    if (!m) throw new Error(`${what}: "${line}" is not "id = name"`);
    out[m[1]] = m[2].trim();
  }
  return out;
}

function namesCard(systems) {
  const c = card('Names');
  for (const sys of systems) {
    const tgs = h('textarea', { class: 'input', rows: 6, 'aria-label': `${sys.label} talkgroup names` });
    tgs.value = namesText(sys.talkgroups);
    const radios = h('textarea', { class: 'input', rows: 6, 'aria-label': `${sys.label} radio names` });
    radios.value = namesText(sys.radios);
    const save = h('button', { class: 'btn', type: 'button', text: 'Save names' });
    save.addEventListener('click', async () => {
      try {
        await api.saveNames(sys.id, { talkgroups: parseNames(tgs.value, 'talkgroups'), radios: parseNames(radios.value, 'radios') });
        toast(`${sys.label}: names saved`);
      } catch (e) {
        toast(e.message, true);
      }
    });
    c.body.append(h('h3', { text: sys.label }),
      h('div', { class: 'grid-2' },
        h('label', { class: 'stack' }, h('span', { class: 'dim', text: 'Talkgroups, one "id = name" a line' }), tgs),
        h('label', { class: 'stack' }, h('span', { class: 'dim', text: 'Radios' }), radios)),
      h('div', { class: 'row end' }, save));
  }
  return c.el;
}

// After an import or a reset the scanner restarts: reload once it answers again.
function reloadAfterRestart() {
  const started = Date.now();
  const poll = async () => {
    try {
      const s = await api.status();
      if (s.uptime_s < 60 && Date.now() - started > 2000) {
        location.reload();
        return;
      }
    } catch (e) {
      // Restarting.
    }
    if (Date.now() - started < 60000) setTimeout(poll, 1000);
  };
  setTimeout(poll, 1500);
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
    if (!confirm(`Replace the radio settings, systems, sites and profiles with ${f.name}? The radio restarts.`)) return;
    try {
      const r = await api.importConfig(doc);
      toast(`Imported ${r.systems} systems, ${r.sites} sites, ${r.profiles} profiles; restarting`);
      reloadAfterRestart();
    } catch (e) {
      toast(e.message, true);
    }
  });
  const reset = h('button', { class: 'btn danger', type: 'button', text: 'Factory reset' });
  reset.addEventListener('click', async () => {
    if (!confirm('Factory reset: delete every system, site, profile, recording and the call history, and restore the default settings? The crystal calibration stays. This cannot be undone.')) return;
    try {
      const r = await api.factoryReset();
      toast(`Reset: ${r.sites} sites, ${r.recordings} recordings, ${r.calls} calls removed; restarting`);
      reloadAfterRestart();
    } catch (e) {
      toast(e.message, true);
    }
  });
  c.body.append(
    h('p', { class: 'card-note', text: 'The radio settings, systems with their names and sites, and profiles, as one file. What the radio learned on the air and the crystal calibration stay with the board.' }),
    h('div', { class: 'row' }, exportLink, file, importBtn),
    h('div', { class: 'row end' }, reset));
  return c.el;
}

export function mount(el) {
  const host = h('div', { class: 'stack' });
  el.append(host);
  let crystal = null;
  Promise.all([api.radio(), api.crystal(), api.systems(), api.recordings(0)])
    .then(([radio, crystalStatus, systems, recs]) => {
      crystal = crystalCard(crystalStatus);
      host.append(gainCard(radio), radioCard(radio), crystal.el, recordingCard(radio, recs), configCard(), profilesCard(systems), namesCard(systems));
    })
    .catch(e => toast(e.message, true));
  return { update() {}, unmount() { if (crystal) crystal.stop(); } };
}
