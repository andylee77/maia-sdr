// Change 068: talkgroups never followed — the opposite of the monitor
// list, and it wins over it and over the speaker groups. A talkgroup on
// the air when it is added is dropped at once. Grants of ignored
// talkgroups still appear in Recent calls (not followed: "ignored").

import { h, setText, card, toast } from '../dom.js';
import { parseTgList, formatTgList } from '../format.js';

export function ignoreList({ onSave }) {
  const c = card('Ignored talkgroups');
  const count = h('span', { class: 'dim', style: { fontSize: '0.84em' } });
  const chips = h('div', { class: 'row', style: { flexWrap: 'wrap', gap: '6px' } });
  const input = h('input', { class: 'input', type: 'text', placeholder: 'e.g. 402, 700-710', 'aria-label': 'Talkgroups to ignore' });
  const add = h('button', { class: 'btn small', type: 'button', text: 'Ignore' });
  const clear = h('button', { class: 'btn small', type: 'button', text: 'Clear' });
  c.right.append(count);
  c.body.append(
    chips,
    h('div', { class: 'row', style: { marginTop: '10px' } }, input, add, h('div', { class: 'spacer' }), clear),
    h('p', { class: 'card-note', text: 'These talkgroups are never followed, even when they are on the monitor list or in a speaker group. One on the air when you add it is dropped at once. Their grants still show in Recent calls as not followed (ignored). Saved on the radio.' }),
  );

  let list = [];
  let aliases = {};

  async function store(next) {
    try {
      await onSave(next);
      list = next;
      render();
    } catch (e) {
      toast('Ignore list not changed: ' + e.message, true);
    }
  }

  function render() {
    chips.replaceChildren(...list.map(tg => h('button', {
      class: 'chip', type: 'button', title: 'Stop ignoring TG ' + tg,
      onclick: () => store(list.filter(t => t !== tg)),
    }, (aliases[tg] ? aliases[tg] + ' (' + tg + ')' : 'TG ' + tg) + ' ✕')));
    if (!list.length) chips.append(h('span', { class: 'dim', text: 'Nothing ignored.' }));
    setText(count, list.length ? formatTgList(list) : '');
    clear.disabled = !list.length;
  }

  add.addEventListener('click', () => {
    const { tgs, bad } = parseTgList(input.value);
    if (bad.length) { toast('Not a talkgroup: ' + bad.join(', '), true); return; }
    if (!tgs.length) return;
    input.value = '';
    store([...new Set([...list, ...tgs])].sort((a, b) => a - b));
  });
  input.addEventListener('keydown', e => { if (e.key === 'Enter') add.click(); });
  clear.addEventListener('click', () => { if (confirm('Stop ignoring every talkgroup?')) store([]); });

  function set(next, tgAliases) {
    list = (next || []).slice();
    aliases = tgAliases || {};
    render();
  }

  render();
  return { el: c.el, set };
}
