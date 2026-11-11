// What the diagnostics pages share (the radio's and each module's): a card of names and values,
// and the main pane, which is one part's lines of the event log as the radio writes them, with
// what else the page puts behind the same tabs.

import { h, card, setClass, switchInput } from '../dom.js';
import { clockMs } from '../format.js';
import { want } from '../store.js';

// Lines kept in an events box.
const KEEP = 500;

export const kv = rows => h('dl', { class: 'kv' }, ...rows.flatMap(([k, v]) => [h('dt', { text: k }), h('dd', { text: v })]));

// A card of [name, value] rows, drawn again only when a row changed; with no rows, the line
// `none`.
export function kvCard(title) {
  const c = card(title);
  let drawn;
  return {
    el: c.el,
    show(rows, none) {
      const key = JSON.stringify(rows || none);
      if (key === drawn) return;
      drawn = key;
      c.body.replaceChildren(rows ? kv(rows) : h('p', { class: 'dim', text: none }));
    },
  };
}

// A line of the log: when, what wrote it (a control channel's protocol, or what the radio did
// by its kind), and its text.
function line(e) {
  const did = e.source === 'system';
  return h('div', { class: 'log-line' + (e.valid ? '' : ' dim') },
    h('span', { class: 't', text: clockMs(e.unix_ms) }),
    h('span', { class: 'c c-' + (did ? 'system' : 'cc'), text: did ? e.class : e.source }),
    h('span', { class: 'm', text: e.text }));
}

// One part's events ('scanner', 'tv' or 'radio'), each line as the radio writes it.
function eventLog(part) {
  const el = h('div', { class: 'log', role: 'log' });
  // `after` moves on as lines come, so a resubscription does not send them again.
  const wants = { events: { after: 0, routine: false, part } };
  function lines(m) {
    if (m.type === 'restart') {
      el.replaceChildren();
      return;
    }
    const list = m.events;
    // Lines sent for the page just left can still arrive: they are another part's.
    if (!list.length || list[0].part !== part) return;
    const pinned = el.scrollTop + el.clientHeight >= el.scrollHeight - 4;
    for (const e of list) el.append(line(e));
    wants.events.after = list[list.length - 1].seq;
    while (el.childElementCount > KEEP) el.firstElementChild.remove();
    if (pinned) el.scrollTop = el.scrollHeight;
  }
  let stop = want(wants, lines);
  return {
    el,
    // The housekeeping lines too, or not: the log is read afresh.
    routine(on) {
      stop();
      wants.events = { after: 0, routine: on, part };
      el.replaceChildren();
      stop = want(wants, lines);
    },
    stop: () => stop(),
  };
}

// A diagnostics page's main pane: `part`'s events, and the page's other panes ([key, label, el])
// behind tabs. `state.tab` is the page's own, kept while it is left for another. `housekeeping`
// adds the switch that shows the control channel's housekeeping broadcasts too.
export function mainPane(part, state, { label, panes = [], housekeeping = false }) {
  const log = eventLog(part);
  const all = [['events', 'Events', log.el], ...panes];
  const keep = housekeeping ? switchInput('Housekeeping', false, on => log.routine(on)) : null;
  const chips = all.map(([key, text]) => h('button', {
    class: 'chip', type: 'button', text,
    onclick: () => {
      state.tab = key;
      show();
    },
  }));
  const el = h('section', { class: 'card flush diag-main', 'aria-label': label },
    h('div', { class: 'card-head' },
      all.length > 1 ? h('div', { class: 'row', role: 'group', 'aria-label': 'What to show' }, ...chips) : h('h2', { text: 'Events' }),
      h('div', { class: 'spacer' }), keep ? keep.el : null),
    h('div', { class: 'card-body' }, ...all.map(p => p[2])));
  function show() {
    all.forEach(([key, , pane], i) => {
      setClass(chips[i], 'on', key === state.tab);
      chips[i].setAttribute('aria-pressed', String(key === state.tab));
      pane.hidden = key !== state.tab;
    });
    if (keep) keep.el.hidden = state.tab !== 'events';
    // A log shown again is read from its end.
    if (state.tab === 'events') log.el.scrollTop = log.el.scrollHeight;
  }
  show();
  return { el, stop: log.stop };
}

// The line over a page's cards saying where the other diagnostics are.
export const elsewhere = (...parts) => h('p', { class: 'card-note diag-where' }, ...parts);
