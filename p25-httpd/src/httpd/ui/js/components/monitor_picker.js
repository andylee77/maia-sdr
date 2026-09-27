// Talkgroup monitor list (scanner allow-list). Empty = follow every
// clear grant. The roster is every TG seen in a grant since boot
// (/api/grant_map) plus whatever is already on the list.

import { h, setText, card, toast } from '../dom.js';
import { api } from '../api.js';

export function monitorPicker({ onSave }) {
  const c = card('Monitor list');
  const status = h('span', { class: 'dim', style: { fontSize: '0.84em' } });
  const refresh = h('button', { class: 'btn small', type: 'button', text: 'Refresh roster' });
  const clearBtn = h('button', { class: 'btn small', type: 'button', text: 'Clear' });
  const save = h('button', { class: 'btn primary small', type: 'button', text: 'Apply', disabled: true });
  const manual = h('input', { class: 'input num', type: 'number', min: 1, max: 65535, placeholder: 'TG', 'aria-label': 'Add talkgroup' });
  const addBtn = h('button', { class: 'btn small', type: 'button', text: 'Add' });
  const grid = h('div', { style: { display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(190px, 1fr))', gap: '6px' } });
  c.right.append(status, save);
  c.body.append(
    grid,
    h('div', { class: 'row', style: { marginTop: '10px' } }, manual, addBtn, h('div', { class: 'spacer' }), clearBtn, refresh),
    h('p', { class: 'card-note', text: 'With an empty list the follower takes every clear grant. With TGs checked it follows only those (first listed wins when two are granted at once). Encrypted TGs are skipped either way. Saved on the radio and restored at boot.' }),
  );

  let roster = [];      // [{tg, count, enc, alias}]
  let applied = [];     // saved list
  let staged = new Set();
  let aliases = {};

  const same = () => applied.length === staged.size && applied.every(t => staged.has(t));

  function render() {
    const tgs = new Map(roster.map(r => [r.tg, r]));
    for (const t of staged) if (!tgs.has(t)) tgs.set(t, { tg: t, count: 0, enc: 0 });
    const rows = [...tgs.values()].sort((a, b) => b.count - a.count || a.tg - b.tg);
    grid.replaceChildren(...rows.map(r => {
      const cb = h('input', { type: 'checkbox' });
      cb.checked = staged.has(r.tg);
      cb.addEventListener('change', () => { if (cb.checked) staged.add(r.tg); else staged.delete(r.tg); refreshButtons(); });
      const name = aliases[r.tg] ? aliases[r.tg] + ' (' + r.tg + ')' : 'TG ' + r.tg;
      const meta = (r.count ? r.count + ' grants' : 'not seen') + (r.enc ? ' · ' + r.enc + ' enc' : '');
      return h('label', { class: 'chip', style: { justifyContent: 'flex-start', borderRadius: '8px', padding: '6px 10px' } },
        cb, h('span', { text: name }), h('span', { class: 'faint', style: { marginLeft: 'auto' }, text: meta }));
    }));
    if (!rows.length) grid.append(h('span', { class: 'dim', text: 'No talkgroups seen yet.' }));
    refreshButtons();
  }

  function refreshButtons() {
    save.disabled = same();
    setText(status, applied.length ? 'following ' + applied.length + ' TG' + (applied.length > 1 ? 's' : '') : 'following all clear TGs');
    if (!same()) setText(status, 'unsaved changes');
  }

  async function loadRoster() {
    try {
      const gm = await api.grantMap();
      const by = new Map();
      for (const e of gm.entries || []) {
        const r = by.get(e.tg) || { tg: e.tg, count: 0, enc: 0 };
        r.count += e.count;
        r.enc += e.encrypted_count;
        by.set(e.tg, r);
      }
      roster = [...by.values()];
      render();
    } catch (e) {
      toast('Roster unavailable: ' + e.message, true);
    }
  }

  addBtn.addEventListener('click', () => {
    const tg = parseInt(manual.value, 10);
    if (!Number.isFinite(tg) || tg < 1 || tg > 65535) { toast('TG must be 1–65535', true); return; }
    staged.add(tg);
    manual.value = '';
    render();
  });
  clearBtn.addEventListener('click', () => { staged.clear(); render(); });
  refresh.addEventListener('click', loadRoster);
  save.addEventListener('click', async () => {
    save.disabled = true;
    try {
      // Keep the saved priority order; new TGs go last.
      const list = applied.filter(t => staged.has(t))
        .concat([...staged].filter(t => !applied.includes(t)).sort((a, b) => a - b));
      await onSave(list);
      applied = list;
      refreshButtons();
      toast('Monitor list applied');
    } catch (e) {
      toast('Apply failed: ' + e.message, true);
      refreshButtons();
    }
  });

  function set(list, tgAliases) {
    aliases = tgAliases || {};
    const wasSame = same();
    applied = (list || []).slice();
    if (wasSame) staged = new Set(applied);
    render();
  }

  loadRoster();
  return { el: c.el, set };
}
