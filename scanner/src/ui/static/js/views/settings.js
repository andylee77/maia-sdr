// Settings: the receiver gain and each site's active profile.

import { h, card, toast } from '../dom.js';
import { api } from '../api.js';

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
  Promise.all([api.radio(), api.systems(), api.profiles()])
    .then(([radio, systems, profiles]) => host.append(gainCard(radio), profilesCard(systems, profiles)))
    .catch(e => toast(e.message, true));
  return { update() {}, unmount() {} };
}
