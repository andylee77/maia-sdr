// The Aircraft module's Plot page while it is not built: it says what it will hold, so the
// module's tabs are already what they will be.

import { h, card } from '../dom.js';

function page(title, why, will) {
  return {
    mount(el) {
      const c = card(title);
      c.body.append(h('p', { class: 'card-note', text: why }), h('p', { class: 'shell-will', text: will }));
      const wrap = h('div', { class: 'shell-page' }, c.el);
      el.append(wrap);
      return { update() {}, unmount() { wrap.remove(); } };
    },
  };
}

export const plot = page('Plot', 'Not built yet: a plot needs to know where the radio is, which it does not.',
  'The aircraft around the radio on a plot with range rings, each with its track; no map from the Internet.');
