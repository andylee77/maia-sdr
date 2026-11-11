// The radio's own settings (the rail's Settings): who may reach it (access.js), where its clock
// comes from, the crystal's correction, and the configuration as a whole (export, import,
// factory reset). What belongs to one module is among that module's tabs: the Scanner's settings
// are scanner_settings.js.

import { h, toast, switchInput } from '../dom.js';
import { api } from '../api.js';
import { store, subscribe, want, boardNow } from '../store.js';
import { ago, num } from '../format.js';
import { crystalSource } from '../protocols.js';
import { row, unit, count, typed, foot, section, settingsPage } from './settings_parts.js';
import { accessCard } from './access.js';

function clockCard() {
  const sec = section('Clock');
  const source = h('select', { class: 'input', 'aria-label': 'Clock' },
    ...[['site', 'The control channel'], ['ntp', 'The internet'], ['manual', 'By hand']].map(([v, t]) => h('option', { value: v, text: t })));
  // What the radio holds, to go back to when a pick is refused.
  let kept = null;
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
  sec.body.append(
    row('The clock comes from', source, 'Saved as it is picked.'),
    row('Set it to this browser\'s time', setNow));
  return {
    el: sec.el,
    show({ radio }) {
      kept = radio.config.clock.source;
      source.value = kept;
    },
  };
}

const ppm = v => `${v >= 0 ? '+' : ''}${v.toFixed(4)} ppm`;
const signedHz = v => `${v >= 0 ? '+' : ''}${Math.round(v)} Hz`;

// The crystal correction, pushed by the radio while the page is open.
function crystalCard() {
  const sec = section('Crystal');
  const lines = h('dl', { class: 'kv' });
  const anchor = count(0, 1000, 'Anchor, Hz');
  sec.edits(anchor);
  // True once the radio holds it.
  const save = async on => {
    try {
      const s = await api.setCrystal({ tracking: on, anchor_hz: typed(anchor, 'The anchor') });
      sec.saved();
      status(s);
      toast('Crystal tracking saved');
      return true;
    } catch (e) {
      toast(e.message, true);
      return false;
    }
  };
  const tracking = switchInput('', false, async on => {
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
      status(await api.crystal());
    } catch (e) {
      toast(e.message, true);
    } finally {
      calibrate.disabled = false;
    }
  });
  // What is measured: at each push.
  function status(s) {
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
  sec.body.append(
    lines,
    row('Follow its drift', tracking.el),
    row('…within', [anchor, unit('Hz of the calibration'), saveAnchor]),
    foot('The crystal is the board\'s, and its correction serves every module. It is measured on the scanner\'s live control channel: once the site is decoded after start, and then followed as it warms and cools.', calibrate));
  const stop = want({ crystal: true }, m => status(m.crystal));
  return {
    el: sec.el,
    stop,
    show({ crystal }) {
      status(crystal);
      tracking.input.checked = crystal.tracking;
      if (!sec.touched) anchor.value = crystal.anchor_hz;
    },
  };
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
  const sec = section('Configuration');
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
  sec.body.append(
    row('Export', exportLink, 'Every module\'s settings and the systems with their aliases and sites, as one file. What the radio learned on the air and the crystal calibration stay with the board.'),
    row('Import', [file, importBtn], 'Replaces the settings, systems, sites and aliases with a file\'s; the radio restarts.'),
    row('Factory reset', reset, 'Deletes every system, site, recording, the call history and the TV channel list, and restores the default settings. The crystal calibration stays.'));
  return { el: sec.el, show() {} };
}

export function mount(el) {
  return settingsPage(el, {
    read: async () => {
      const [radio, crystal, me] = await Promise.all([api.radio(), api.crystal(), api.session()]);
      // The passwords, sessions and tokens are the administrator's to see.
      const access = me.protected && me.level === 'admin' ? await api.access() : null;
      return { radio, crystal, me, access };
    },
    build: (data, reload) => [[accessCard(reload)], [clockCard(), crystalCard(), configCard()]],
    rev: s => `${s.radioRev}.${s.accessRev}`,
    link: ['These are the radio\'s own: who may reach it, its clock, its crystal, its configuration. The scanner\'s settings (its gain, recording, call timings, the history) are under ', h('a', { href: '#scanner-settings', text: 'Scanner, Settings' }), '.'],
  });
}
