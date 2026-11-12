// The Aircraft module's pages while the mode is only its shell: it takes the radio and gives it
// back, and nothing listens yet. Each page says what it will hold, so the module's place in the
// rail and its tabs are already what they will be.

import { h, card } from '../dom.js';

const NOT_YET = 'Not built yet. The Aircraft module can take the radio and give it back; while it has the radio nothing listens.';

// A page that says what it will hold.
function page(title, will) {
  return {
    mount(el) {
      const c = card(title);
      c.body.append(h('p', { class: 'card-note', text: NOT_YET }), h('p', { class: 'shell-will', text: will }));
      const wrap = h('div', { class: 'shell-page' }, c.el);
      el.append(wrap);
      return { update() {}, unmount() { wrap.remove(); } };
    },
  };
}

export const aircraft = page('Aircraft', 'Every aircraft heard: its address and callsign, its altitude, speed and heading, where it is and how far, how strong and when it was last heard.');
export const plot = page('Plot', 'The aircraft around the radio on a plot with range rings, each with its track; no map from the Internet.');
