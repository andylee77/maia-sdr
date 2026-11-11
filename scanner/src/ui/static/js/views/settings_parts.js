// What the settings pages share (the radio's and each module's): a setting's row, a card that
// knows when it holds edits not saved yet, and the page that reads what the radio holds into its
// cards and reads it again when a setting changes, here or from another browser (the radio
// announces each change). A card saves its own part and no other, so two pages never write over
// each other.

import { h, card, toast } from '../dom.js';

// A setting's row: its name, a note under it, and its control (one node or several).
export function row(label, control, note) {
  return h('div', { class: 'set-row' },
    h('div', { class: 'set-what' }, h('div', { text: label }), note ? h('div', { class: 'sub', text: note }) : null),
    h('div', { class: 'set-ctl' }, ...[].concat(control)));
}

export const unit = text => h('span', { class: 'dim', text });
export const count = (min, max, label) => h('input', { class: 'input num', type: 'number', min, max, step: 1, 'aria-label': label });

// The number typed in `input`. An empty box or other text is refused, not read as 0.
export function typed(input, what, { whole = true } = {}) {
  const text = String(input.value).trim();
  const n = Number(text);
  if (!text || !Number.isFinite(n) || (whole && !Number.isInteger(n))) throw new Error(`${what}: ${whole ? 'a whole number' : 'a number'}`);
  return n;
}

// A card's last line: a note on the left (its text, or the element a card keeps to rewrite), its
// buttons on the right.
export function foot(note, ...buttons) {
  const left = note instanceof Node ? note : h('p', { class: 'card-note', text: note || '' });
  return h('div', { class: 'set-foot' }, left, h('div', { class: 'set-ctl' }, ...buttons));
}

// A settings card. Its fields that wait for a Save are named with `edits(...)`: once one is
// changed the card is `touched`, and what the radio holds is not put over what is being typed
// until `saved()`.
export function section(title) {
  const c = card(title);
  let touched = false;
  return {
    el: c.el,
    body: c.body,
    edits(...fields) {
      for (const f of fields) {
        f.addEventListener('input', () => { touched = true; });
        f.addEventListener('change', () => { touched = true; });
      }
    },
    get touched() { return touched; },
    saved() { touched = false; },
  };
}

// A settings page in two columns. `read()` gives what its cards show; `build(data, reload)`
// gives the cards, [left, right], each { el, show(data), stop? } (`reload()` reads the page
// again). `link` is a line over them saying where the other settings are. `rev(store)` is what
// moves when the page's settings changed on the radio: the radio's settings unless said.
export function settingsPage(el, { read, build, link, rev = s => s.radioRev }) {
  const left = h('div', { class: 'stack' });
  const right = h('div', { class: 'stack' });
  el.append(h('div', { class: 'settings' }, link ? h('p', { class: 'card-note set-where' }, ...link) : null, left, right));
  let cards = null;
  // The page was left before the radio answered: nothing is built, so nothing is left asking.
  let gone = false;
  let reading = false;
  let again = false;
  // The store's count of settings changes the cards were last read at.
  let seen = null;

  async function load() {
    if (reading) {
      again = true;
      return;
    }
    reading = true;
    try {
      const data = await read();
      if (gone) return;
      if (!cards) {
        cards = build(data, load);
        left.append(...cards[0].map(c => c.el));
        right.append(...cards[1].map(c => c.el));
      }
      for (const c of cards.flat()) c.show(data);
    } catch (e) {
      toast(e.message, true);
    } finally {
      reading = false;
      if (again && !gone) {
        again = false;
        load();
      }
    }
  }
  load();

  return {
    update(s) {
      const now = rev(s);
      if (seen === null) seen = now;
      else if (now !== seen) {
        seen = now;
        load();
      }
    },
    unmount() {
      gone = true;
      for (const c of (cards || []).flat()) if (c.stop) c.stop();
    },
  };
}
