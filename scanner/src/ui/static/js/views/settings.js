// Settings: the receiver gain, recording, and each site's active profile.

import { h, card, toast, switchInput } from '../dom.js';
import { api } from '../api.js';
import { bytes, num } from '../format.js';

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

function profilesCard(systems, profiles) {
  const c = card('Profiles');
  for (const sys of systems) {
    const own = profiles.profiles.filter(p => p.system === sys.id);
    for (const site of sys.sites) {
      const select = h('select', { class: 'input' }, ...own.map(p => h('option', { value: p.id, text: p.name })));
      select.value = profiles.active[site.id] || '';
      select.addEventListener('change', async () => {
        try {
          await api.selectProfile(site.id, select.value);
          toast(`${site.label}: ${select.selectedOptions[0].textContent}`);
        } catch (e) {
          toast(e.message, true);
        }
      });
      c.body.append(h('div', { class: 'row' }, h('span', { text: `${sys.label} · ${site.label}` }), h('div', { class: 'spacer' }), select));
    }
  }
  return c.el;
}

export function mount(el) {
  const host = h('div', { class: 'stack' });
  el.append(host);
  Promise.all([api.radio(), api.systems(), api.profiles(), api.recordings(0)])
    .then(([radio, systems, profiles, recs]) => host.append(gainCard(radio), recordingCard(radio, recs), profilesCard(systems, profiles)))
    .catch(e => toast(e.message, true));
  return { update() {}, unmount() {} };
}
