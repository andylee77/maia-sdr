// Change 069: profiles. A profile is a named setup of the talkgroup
// groups, the speakers, the monitor list and the ignored talkgroups;
// each site keeps its own profiles and its own talkgroup and radio
// names (numbers mean different things on different systems). Picking
// a profile, or switching site on the Radio page, swaps the whole setup
// at once; a call the new setup does not follow is dropped.
//
// profilePicker({ manage, onChange }): the picker (Speakers panel), plus
// New / Copy / Rename / Delete when `manage` (Settings → Profiles).
// set(doc) takes the GET /api/ui/settings document.

import { h, setText, toast } from '../dom.js';
import { api } from '../api.js';

export function profilePicker({ manage = false, onChange } = {}) {
  const select = h('select', { class: 'input', 'aria-label': 'Profile' });
  const site = h('span', { class: 'dim', style: { fontSize: '0.84em' } });
  const btn = (text, title, fn) => h('button', { class: 'btn small', type: 'button', text, title, onclick: fn });
  const buttons = manage ? [
    btn('New', 'A new empty profile (follows everything)', () => create(false)),
    btn('Copy', 'A new profile starting from this one', () => create(true)),
    btn('Rename', 'Rename this profile', rename),
    btn('Delete', 'Delete this profile', remove),
  ] : [];
  const el = h('div', { class: 'row', style: { flexWrap: 'wrap', gap: '6px' } },
    h('span', { text: 'Profile' }), select, ...buttons, h('div', { class: 'spacer' }), site);

  let prof = null;
  let busy = false;

  async function act(profile, what) {
    if (busy) return false;
    busy = true;
    try {
      const r = await api.putSettings({ profile });
      if (!r.persisted) toast('Applied, but NOT saved to flash', true);
      if (onChange) await onChange();
      return true;
    } catch (e) {
      toast(what + ': ' + e.message, true);
      render();
      return false;
    } finally {
      busy = false;
    }
  }

  select.addEventListener('change', () => {
    if (prof && select.value !== prof.active) act({ select: select.value }, 'Profile not switched');
  });

  function create(copy) {
    const name = prompt(copy ? 'Name of the copy of "' + prof.active + '":' : 'Name of the new profile:');
    if (name && name.trim()) act({ create: { name: name.trim(), copy } }, 'Profile not created');
  }

  function rename() {
    const name = prompt('New name for "' + prof.active + '":', prof.active);
    if (name && name.trim() && name.trim() !== prof.active) act({ rename: { from: prof.active, to: name.trim() } }, 'Profile not renamed');
  }

  function remove() {
    if (prof.names.length < 2) { toast('A site keeps at least one profile', true); return; }
    if (confirm('Delete the profile "' + prof.active + '"? Its groups, speakers, monitor and ignore lists go with it.')) {
      act({ delete: prof.active }, 'Profile not deleted');
    }
  }

  function render() {
    if (!prof) return;
    select.replaceChildren(...prof.names.map(n => h('option', { value: n, text: n })));
    select.value = prof.active;
    setText(site, prof.site_label || prof.site || '');
    for (const b of buttons) b.disabled = false;
    if (buttons.length) buttons[3].disabled = prof.names.length < 2;
  }

  function set(doc) {
    prof = doc && doc.profiles && doc.profiles.names ? doc.profiles : null;
    el.hidden = !prof;
    render();
  }

  el.hidden = true;
  return { el, set };
}
