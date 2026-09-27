// Key / value list (<dl class="kv">) updated in place.
//
//   const kv = kvTable();
//   kv.set([['NAC', '8A1'], ['Rate', '38.8 /s', 'good'], ['--', 'Section']]);
//
// Rows are [label, value, cls?]; a label of '--' renders a section
// separator using the value as its title.

import { h, setText } from '../dom.js';

export function kvTable() {
  const el = h('dl', { class: 'kv' });
  let sig = '';
  const cells = [];

  function set(rows) {
    const nextSig = rows.map(r => r[0]).join('\u0001');
    if (nextSig !== sig) {
      el.replaceChildren();
      cells.length = 0;
      for (const [label, value] of rows) {
        if (label === '--') {
          const s = h('div', { class: 'sep', text: value });
          el.appendChild(s);
          cells.push(null);
        } else {
          const dd = h('dd');
          el.append(h('dt', { text: label }), dd);
          cells.push(dd);
        }
      }
      sig = nextSig;
    }
    rows.forEach((r, i) => {
      const dd = cells[i];
      if (!dd) return;
      setText(dd, r[1]);
      const cls = r[2] || '';
      if (dd.className !== cls) dd.className = cls;
    });
  }

  return { el, set };
}

// Small "label over value" metric used in strips.
export function metric(label) {
  const value = h('span', { class: 'm-value', text: '—' });
  const el = h('div', { class: 'metric' }, h('span', { class: 'm-label', text: label }), value);
  return { el, set: v => setText(value, v) };
}
