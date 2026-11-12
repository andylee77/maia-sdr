// Data's Devices page while it is not built: it says what it will hold, so the module's tabs are
// already what they will be. Scan lists what the mode hears by frequency meanwhile.

import { h, card } from '../dom.js';

// A page that says what it will hold.
function page(title, now, will) {
  return {
    mount(el) {
      const c = card(title);
      c.body.append(h('p', { class: 'card-note', text: now }), h('p', { class: 'data-will', text: will }));
      const wrap = h('div', { class: 'data-shell' }, c.el);
      el.append(wrap);
      return { update() {}, unmount() { wrap.remove(); } };
    },
  };
}

export const devices = page('Devices',
  'Not built yet. Data mode decodes nothing so far: Scan lists what it hears as emitters, by frequency.',
  'Every device and frequency heard, each with its data log.');
