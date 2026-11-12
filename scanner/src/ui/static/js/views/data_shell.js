// Data's three pages while the mode is only its shell: it takes the radio and gives it back, and
// nothing listens yet. Each page says what it will hold, so the module's place in the rail and
// its tabs are already what they will be.

import { h, card } from '../dom.js';

const NOT_YET = 'Not built yet. Data mode can take the radio and give it back; while it has the radio nothing listens.';

// A page that says what it will hold.
function page(title, will) {
  return {
    mount(el) {
      const c = card(title);
      c.body.append(h('p', { class: 'card-note', text: NOT_YET }), h('p', { class: 'data-will', text: will }));
      const wrap = h('div', { class: 'data-shell' }, c.el);
      el.append(wrap);
      return { update() {}, unmount() { wrap.remove(); } };
    },
  };
}

export const scan = page('Scan', 'The window being scanned, as a waterfall and a spectrum, and the emitters heard in it: this is where an emitter is named.');
export const devices = page('Devices', 'Every device and frequency heard, each with its data log.');
export const captures = page('Captures', 'The IQ kept of bursts and of whole windows, as files to download.');
