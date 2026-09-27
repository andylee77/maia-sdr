// Change 063: talkgroup groups (Settings). A group is a name and a
// talkgroup list typed like "301-310, 315"; the order of the list is the
// priority order the follower uses (1 = highest). Which group plays on
// which speaker is set on the Now page ("Speakers"). Saved on the radio.

import { h, card, toast } from '../dom.js';
import { parseTgList, formatTgList } from '../format.js';

export function tgGroupsEditor({ onSave }) {
  const c = card('Talkgroup groups');
  const save = h('button', { class: 'btn primary small', type: 'button', text: 'Save', disabled: true });
  const add = h('button', { class: 'btn small', type: 'button', text: 'Add group' });
  const list = h('div', { class: 'grp-edit' });
  c.right.append(save);
  c.body.append(
    list,
    h('div', { class: 'row', style: { marginTop: '10px' } }, add),
    h('p', { class: 'card-note', text: 'List order is priority: when two groups are granted at once the higher one is followed, and with interrupting on (Now → Speakers) it takes the channel from a lower one. Talkgroups: numbers and ranges, e.g. 301-310, 315. Pick the speaker for each group on the Now page.' }));

  // rows: [{orig, name, tgsText}] — `orig` is the saved name, so a rename
  // keeps the group on its speaker.
  let rows = [];
  let saved = '';
  let doc = null;

  const snapshot = () => JSON.stringify(rows.map(r => [r.name.trim(), r.tgsText.trim()]));
  const dirty = () => { save.disabled = snapshot() === saved; };

  function render() {
    list.replaceChildren(...rows.map((r, i) => {
      const name = h('input', { class: 'input', type: 'text', value: r.name, placeholder: 'Name', 'aria-label': 'Group name', maxlength: 32 });
      name.addEventListener('input', () => { r.name = name.value; dirty(); });
      const tgs = h('input', { class: 'input grp-tgs', type: 'text', value: r.tgsText, placeholder: '300, 301-310', 'aria-label': 'Talkgroups' });
      tgs.addEventListener('input', () => { r.tgsText = tgs.value; dirty(); });
      const move = d => () => { const j = i + d; [rows[i], rows[j]] = [rows[j], rows[i]]; render(); dirty(); };
      const up = h('button', { class: 'btn small icon', type: 'button', title: 'Higher priority', text: '↑', disabled: i === 0, onclick: move(-1) });
      const down = h('button', { class: 'btn small icon', type: 'button', title: 'Lower priority', text: '↓', disabled: i === rows.length - 1, onclick: move(1) });
      const del = h('button', { class: 'btn small icon', type: 'button', title: 'Delete group', text: '✕', onclick: () => { rows.splice(i, 1); render(); dirty(); } });
      return h('div', { class: 'grp-row' }, h('span', { class: 'grp-rank', text: String(i + 1) }), name, tgs, up, down, del);
    }));
    if (!rows.length) list.append(h('div', { class: 'dim', text: 'No groups yet. Add one, e.g. Primary = 300.' }));
  }

  add.addEventListener('click', () => { rows.push({ orig: null, name: '', tgsText: '' }); render(); dirty(); });

  save.addEventListener('click', async () => {
    const groups = [];
    const rename = {};
    for (const r of rows) {
      const name = r.name.trim();
      const { tgs, bad } = parseTgList(r.tgsText);
      if (!name) { toast('A group has no name', true); return; }
      if (bad.length) { toast(name + ': not a talkgroup: ' + bad.join(', '), true); return; }
      groups.push({ name, tgs });
      if (r.orig && r.orig !== name) rename[r.orig] = name;
    }
    // Keep renamed groups on their speaker (the radio routes by name).
    const sp = (doc && doc.settings.speakers) || { left: [], right: [], other: 'both', preempt: true };
    const keep = names => names.map(n => rename[n] || n).filter(n => groups.some(g => g.name === n));
    const speakers = Object.assign({}, sp, { left: keep(sp.left || []), right: keep(sp.right || []) });
    try {
      save.disabled = true;
      await onSave({ tg_groups: groups, speakers });
      toast('Groups saved');
    } catch (e) {
      save.disabled = false;
      toast(e.message, true);
    }
  });

  function set(d) {
    doc = d;
    if (!save.disabled) return; // unsaved edits win over a reload
    rows =(d.settings.tg_groups || []).map(g => ({ orig: g.name, name: g.name, tgsText: formatTgList(g.tgs) }));
    saved = snapshot();
    render();
    dirty();
  }

  return { el: c.el, set };
}
