// Editable id -> name table (talkgroup or radio-unit aliases).
//
//   const ed = aliasEditor({ title, idLabel, maxId, onSave: async map => ... });
//   ed.set(map)   // {"300": "EMS Dispatch", ...}
//
// Saving sends the whole map (PUT /api/ui/settings replaces it).

import { h, setText, card, toast } from '../dom.js';

export function aliasEditor({ title, idLabel, maxId, maxChars = 48, note, onSave }) {
  const c = card(title);
  const tbody = h('tbody');
  const newId = h('input', { class: 'input num', type: 'number', min: 1, max: maxId, placeholder: idLabel, 'aria-label': 'New ' + idLabel });
  const newName = h('input', { class: 'input wide', type: 'text', maxlength: maxChars, placeholder: 'Name', 'aria-label': 'New name' });
  const add = h('button', { class: 'btn small', type: 'button', text: 'Add' });
  const save = h('button', { class: 'btn primary small', type: 'button', text: 'Save', disabled: true });
  const status = h('span', { class: 'dim', style: { fontSize: '0.84em' } });
  c.right.append(status, save);
  c.body.append(
    h('div', { class: 'table-wrap' }, h('table', { class: 'table' },
      h('thead', null, h('tr', null, h('th', { text: idLabel }), h('th', { text: 'Name' }), h('th'))),
      tbody)),
    h('div', { class: 'row', style: { marginTop: '10px' } }, newId, newName, add),
  );
  if (note) c.body.append(h('p', { class: 'card-note', text: note }));

  let saved = {};
  let draft = {};

  function dirty() {
    const a = JSON.stringify(Object.entries(saved).sort());
    const b = JSON.stringify(Object.entries(draft).sort());
    return a !== b;
  }

  function render() {
    const ids = Object.keys(draft).map(Number).sort((x, y) => x - y);
    tbody.replaceChildren(...ids.map(id => {
      const name = h('input', { class: 'input wide', type: 'text', maxlength: maxChars, value: draft[id], 'aria-label': 'Name for ' + id });
      name.addEventListener('input', () => { draft[id] = name.value; refreshButtons(); });
      const del = h('button', { class: 'btn small danger', type: 'button', title: 'Remove', text: 'Remove' });
      del.addEventListener('click', () => { delete draft[id]; render(); });
      return h('tr', null, h('td', { class: 'mono', text: String(id) }), h('td', null, name), h('td', null, del));
    }));
    if (!ids.length) tbody.append(h('tr', null, h('td', { colspan: 3, class: 'dim', text: 'None yet.' })));
    refreshButtons();
  }

  function refreshButtons() {
    save.disabled = !dirty();
    setText(status, dirty() ? 'unsaved changes' : Object.keys(saved).length + ' saved');
  }

  add.addEventListener('click', () => {
    const id = parseInt(newId.value, 10);
    const name = newName.value.trim();
    if (!Number.isFinite(id) || id < 1 || id > maxId) { toast(idLabel + ' must be 1–' + maxId, true); return; }
    if (!name) { toast('Enter a name', true); return; }
    draft[id] = name;
    newId.value = ''; newName.value = '';
    render();
  });

  save.addEventListener('click', async () => {
    save.disabled = true;
    const out = {};
    for (const [k, v] of Object.entries(draft)) if (v.trim()) out[k] = v.trim();
    try {
      await onSave(out);
      saved = { ...out };
      draft = { ...out };
      render();
      toast(title + ' saved');
    } catch (e) {
      toast('Save failed: ' + e.message, true);
      refreshButtons();
    }
  });

  // Replace contents from the server unless the user is mid-edit.
  function set(map) {
    const incoming = {};
    for (const [k, v] of Object.entries(map || {})) incoming[k] = v;
    if (dirty()) { saved = incoming; refreshButtons(); return; }
    saved = { ...incoming };
    draft = { ...incoming };
    render();
  }

  render();
  return { el: c.el, set };
}
