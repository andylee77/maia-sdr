// The profiles card (Settings): each site's active profile, and the profile editor: groups in
// priority order with the speaker each plays on, the talkgroups in no group, pre-emption, the
// follow-only and never-follow lists. Saving the live site's profile applies to the next calls.

import { h, card, toast } from '../dom.js';
import { api } from '../api.js';
import { parseTgList, formatTgList } from '../format.js';

const SIDES = [['left', 'Left'], ['right', 'Right'], ['off', 'Not followed']];
const OTHER = [['both', 'Both speakers'], ['left', 'Left'], ['right', 'Right'], ['off', 'Not followed']];

function select(options, value, label) {
  const s = h('select', { class: 'input', 'aria-label': label }, ...options.map(([v, t]) => h('option', { value: v, text: t })));
  s.value = value;
  return s;
}

// Talkgroups as typed, or an error naming the bad parts.
function tgs(input, what) {
  const { tgs: list, bad } = parseTgList(input.value);
  if (bad.length) throw new Error(`${what}: not talkgroups: ${bad.join(', ')}`);
  return list;
}

function editor(profile, onSaved) {
  const name = h('input', { class: 'input wide', type: 'text', value: profile.name, 'aria-label': 'Profile name' });
  const rows = [];
  const groupsBox = h('div', { class: 'stack' });
  const sp = profile.speakers || { left: [], right: [], other: 'both', preempt: true };

  function addGroup(g) {
    const side = sp.left.includes(g.name) ? 'left' : sp.right.includes(g.name) ? 'right' : 'off';
    const row = {
      name: h('input', { class: 'input', type: 'text', value: g.name, 'aria-label': 'Group name' }),
      list: h('input', { class: 'input wide', type: 'text', value: formatTgList(g.talkgroups), 'aria-label': 'Talkgroups' }),
      side: select(SIDES, side, 'Speaker'),
    };
    const move = d => {
      const i = rows.indexOf(row);
      const j = i + d;
      if (j < 0 || j >= rows.length) return;
      rows.splice(i, 1);
      rows.splice(j, 0, row);
      draw();
    };
    row.el = h('div', { class: 'row' }, row.name, row.list, row.side,
      h('button', { class: 'btn small', type: 'button', text: '↑', title: 'Higher priority', onclick: () => move(-1) }),
      h('button', { class: 'btn small', type: 'button', text: '↓', title: 'Lower priority', onclick: () => move(1) }),
      h('button', { class: 'btn small danger', type: 'button', text: '✕', title: 'Remove the group', onclick: () => { rows.splice(rows.indexOf(row), 1); draw(); } }));
    rows.push(row);
  }

  function draw() {
    groupsBox.replaceChildren(...rows.map(r => r.el),
      h('div', { class: 'row' }, h('button', { class: 'btn small', type: 'button', text: 'Add group', onclick: () => { addGroup({ name: '', talkgroups: [] }); draw(); } })));
  }

  for (const g of profile.groups || []) addGroup(g);
  draw();
  const other = select(OTHER, sp.other || 'both', 'Talkgroups in no group');
  const preempt = h('input', { type: 'checkbox' });
  preempt.checked = sp.preempt !== false;
  const monitor = h('input', { class: 'input wide', type: 'text', value: (profile.monitor || []).join(', '), 'aria-label': 'Follow only' });
  const ignore = h('input', { class: 'input wide', type: 'text', value: formatTgList(profile.ignore), 'aria-label': 'Never follow' });
  const save = h('button', { class: 'btn primary', type: 'button', text: 'Save profile' });
  save.addEventListener('click', async () => {
    try {
      const groups = rows.map(r => ({ name: r.name.value.trim(), talkgroups: tgs(r.list, `group ${r.name.value}`) }));
      const side = s => rows.filter(r => r.side.value === s).map(r => r.name.value.trim());
      const body = {
        id: profile.id,
        system: profile.system,
        name: name.value,
        groups,
        speakers: { left: side('left'), right: side('right'), other: other.value, preempt: preempt.checked },
        monitor: tgs(monitor, 'follow only'),
        ignore: tgs(ignore, 'never follow'),
      };
      save.disabled = true;
      const saved = await api.saveProfile(profile.id, body);
      toast(`Saved ${saved.name}`);
      onSaved(saved.id);
    } catch (e) {
      toast(e.message, true);
    } finally {
      save.disabled = false;
    }
  });
  return h('div', { class: 'stack' },
    h('div', { class: 'row' }, h('span', { text: 'Name' }), name),
    h('h3', { text: 'Groups, highest priority first' }),
    groupsBox,
    h('div', { class: 'row' }, h('span', { text: 'Talkgroups in no group' }), other),
    h('label', { class: 'row' }, preempt, h('span', { text: 'A call of a higher group takes a lane from a lower one' })),
    h('div', { class: 'row' }, h('span', { text: 'Follow only (empty: every clear call)' }), monitor),
    h('div', { class: 'row' }, h('span', { text: 'Never follow' }), ignore),
    h('div', { class: 'row end' }, save));
}

export function profilesCard(systems) {
  const c = card('Profiles');
  // The profile shown, per system.
  const chosen = new Map();

  async function load(keep) {
    try {
      const profiles = await api.profiles();
      render(profiles, keep);
    } catch (e) {
      toast(e.message, true);
    }
  }

  function render(profiles, keep) {
    c.body.replaceChildren();
    for (const sys of systems) {
      const own = profiles.profiles.filter(p => p.system === sys.id);
      c.body.append(h('h3', { text: sys.label }));
      for (const site of sys.sites) {
        const s = h('select', { class: 'input', 'aria-label': `${site.label} profile` }, ...own.map(p => h('option', { value: p.id, text: p.name })));
        s.value = profiles.active[site.id] || '';
        s.addEventListener('change', async () => {
          try {
            await api.selectProfile(site.id, s.value);
            toast(`${site.label}: ${s.selectedOptions[0].textContent}`);
          } catch (e) {
            toast(e.message, true);
          }
        });
        c.body.append(h('div', { class: 'row' }, h('span', { text: `${site.label} follows` }), h('div', { class: 'spacer' }), s));
      }
      const pick = h('select', { class: 'input', 'aria-label': 'Profile to edit' }, ...own.map(p => h('option', { value: p.id, text: p.name })));
      const want = own.some(p => p.id === keep) ? keep : chosen.get(sys.id);
      if (want && own.some(p => p.id === want)) pick.value = want;
      const box = h('div');
      const show = () => {
        chosen.set(sys.id, pick.value);
        const p = own.find(x => x.id === pick.value);
        box.replaceChildren(p ? editor(p, id => load(id)) : h('p', { class: 'dim', text: 'No profiles yet.' }));
      };
      pick.addEventListener('change', show);
      const create = h('button', { class: 'btn small', type: 'button', text: 'New profile' });
      create.addEventListener('click', async () => {
        const name = prompt('Name of the new profile');
        if (!name) return;
        try {
          const p = await api.createProfile(sys.id, name, pick.value || null);
          toast(`Created ${p.name} (a copy of the one shown)`);
          load(p.id);
        } catch (e) {
          toast(e.message, true);
        }
      });
      const del = h('button', { class: 'btn small danger', type: 'button', text: 'Delete' });
      del.addEventListener('click', async () => {
        if (!pick.value || !confirm(`Delete the profile ${pick.selectedOptions[0].textContent}?`)) return;
        try {
          await api.deleteProfile(pick.value);
          toast('Deleted');
          chosen.delete(sys.id);
          load();
        } catch (e) {
          toast(e.message, true);
        }
      });
      c.body.append(h('div', { class: 'row' }, h('span', { text: 'Edit' }), pick, create, del), box);
      show();
    }
  }

  load();
  return c.el;
}
