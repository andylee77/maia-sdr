// Settings: the receiver gain, the radio's other settings, recording, profiles and each system's
// talkgroup and radio names.

import { h, card, toast, switchInput } from '../dom.js';
import { api } from '../api.js';
import { bytes, num } from '../format.js';
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
  c.body.append(
    h('div', { class: 'row' }, h('span', { text: 'Window presets the planner may use' }), ...presets.map(p => p.el)),
    h('div', { class: 'row' }, h('span', { text: 'A call closes after' }), hang, h('span', { class: 'dim', text: 'ms with no sign of life, or' }), grace,
      h('span', { class: 'dim', text: 'ms after its end' })),
    h('div', { class: 'row' }, h('span', { text: 'Keep the history' }), days, h('span', { class: 'dim', text: 'days, at most' }), mb, h('span', { class: 'dim', text: 'MB' })),
    h('div', { class: 'row end' }, save));
  return c.el;
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

export function mount(el) {
  const host = h('div', { class: 'stack' });
  el.append(host);
  Promise.all([api.radio(), api.systems(), api.recordings(0)])
    .then(([radio, systems, recs]) => host.append(gainCard(radio), radioCard(radio), recordingCard(radio, recs), profilesCard(systems), namesCard(systems)))
    .catch(e => toast(e.message, true));
  return { update() {}, unmount() {} };
}
