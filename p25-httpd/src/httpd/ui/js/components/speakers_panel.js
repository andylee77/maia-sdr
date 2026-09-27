// Change 063: "Speakers" (Now page). Which talkgroup groups play on the
// left and on the right speaker, where talkgroups in no group go, and
// whether a higher-priority group interrupts a lower one. Groups are
// defined in Settings → Talkgroup groups; their order is the priority
// (the number on each chip). Saved on the radio: every browser and the
// grant follower use the same routing.

import { h, card, switchInput, toast } from '../dom.js';
import { api } from '../api.js';
import { refreshSettings } from '../store.js';
import { formatTgList } from '../format.js';

const OTHER = [['both', 'both speakers'], ['left', 'the left speaker'], ['right', 'the right speaker'], ['off', 'off (not followed)']];

export function speakersPanel() {
  const c = card('Speakers');
  const cols = h('div', { class: 'spk-cols' });
  const other = h('select', { class: 'input', 'aria-label': 'Talkgroups in no group' },
    ...OTHER.map(([v, t]) => h('option', { value: v, text: t })));
  const pre = switchInput('A higher-priority group interrupts a lower one', true, on => apply({ preempt: on }));
  const empty = h('p', { class: 'card-note' }, 'No talkgroup groups yet: create them in ',
    h('a', { href: '#settings', text: 'Settings → Talkgroup groups' }), ' (e.g. Primary = 300, TAC = 301-310, Hospital), then pick a speaker for each here.');
  const controls = h('div', null,
    cols,
    h('div', { class: 'row', style: { marginTop: '10px' } }, h('span', { text: 'Other talkgroups on' }), other),
    h('div', { style: { marginTop: '8px' } }, pre.el));
  c.body.append(empty, controls);
  other.addEventListener('change', () => apply({ other: other.value }));

  let doc = null;
  let busy = false;

  async function apply(change) {
    if (!doc || busy) return;
    busy = true;
    const speakers = Object.assign({}, doc.settings.speakers, change);
    try {
      const r = await api.putSettings({ speakers });
      if (!r.persisted) toast('Applied, but NOT saved to flash', true);
      await refreshSettings();
    } catch (e) {
      toast('Speakers not changed: ' + e.message, true);
      render(doc);
    } finally {
      busy = false;
    }
  }

  function column(side) {
    const sp = doc.settings.speakers;
    const groups = doc.settings.tg_groups || [];
    const rank = name => groups.findIndex(g => g.name === name);
    const mine = (sp[side] || []).slice().sort((a, b) => rank(a) - rank(b));
    const otherSide = side === 'left' ? 'right' : 'left';
    const chips = mine.map(name => {
      const g = groups[rank(name)];
      const move = () => apply({ [side]: sp[side].filter(n => n !== name), [otherSide]: [...sp[otherSide], name] });
      const remove = () => apply({ [side]: sp[side].filter(n => n !== name) });
      return h('div', { class: 'grp-chip', title: 'TG ' + formatTgList(g ? g.tgs : []) },
        h('span', { class: 'grp-rank', text: String(rank(name) + 1) }),
        h('span', { class: 'grp-name', text: name }),
        h('span', { class: 'grp-tgs dim', text: g ? formatTgList(g.tgs) : '' }),
        h('button', { class: 'btn small icon', type: 'button', title: 'Move to the ' + otherSide + ' speaker', text: side === 'left' ? '→' : '←', onclick: move }),
        h('button', { class: 'btn small icon', type: 'button', title: 'Off this speaker (not followed)', text: '✕', onclick: remove }));
    });
    const free = groups.filter(g => !(sp.left || []).includes(g.name) && !(sp.right || []).includes(g.name));
    const add = h('select', { class: 'input', 'aria-label': 'Add a group to the ' + side + ' speaker', disabled: !free.length },
      h('option', { value: '', text: free.length ? '+ add group' : 'all groups placed' }),
      ...free.map(g => h('option', { value: g.name, text: g.name })));
    add.addEventListener('change', () => { if (add.value) apply({ [side]: [...(sp[side] || []), add.value] }); });
    return h('div', { class: 'spk-col' },
      h('div', { class: 'spk-head', text: side === 'left' ? 'Left' : 'Right' }),
      chips.length ? chips : h('div', { class: 'dim', text: 'nothing' }),
      add);
  }

  function render(d) {
    doc = d;
    const ok = d && d.settings;
    const groups = ok ? d.settings.tg_groups || [] : [];
    empty.hidden = !!(ok && groups.length);
    controls.hidden = !ok;
    if (!ok) return;
    const sp = d.settings.speakers;
    cols.replaceChildren(column('left'), column('right'));
    if (document.activeElement !== other) other.value = sp.other;
    pre.input.checked = !!sp.preempt;
  }

  return {
    el: c.el,
    update(kind, store) { if (kind === 'settings' || (!doc && store.settings)) render(store.settings); },
  };
}
