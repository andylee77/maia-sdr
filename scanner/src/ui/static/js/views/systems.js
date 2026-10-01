// Systems: every configured system and its sites; make a site live. (The setup scan joins here.)

import { h, card, toast } from '../dom.js';
import { mhz, DASH } from '../format.js';
import { api } from '../api.js';
import { refresh } from '../store.js';
import { protocol } from '../protocols.js';

function kv(rows) {
  return h('table', { class: 'kv' }, ...rows.map(([k, v]) =>
    h('tr', null, h('th', { text: k }), h('td', { text: v === null || v === undefined ? DASH : v }))));
}

export function mount(el) {
  const list = h('div', { class: 'stack' });
  el.append(list);
  let liveId = null;
  let systems = [];

  async function activate(site, button) {
    button.disabled = true;
    try {
      await api.activate(site.id);
      toast(`${site.label} is live`);
      await refresh();
    } catch (e) {
      toast(e.message, true);
    } finally {
      button.disabled = false;
    }
  }

  function render() {
    list.replaceChildren();
    if (!systems.length) {
      list.append(card('No systems yet').el);
      return;
    }
    for (const sys of systems) {
      const p = protocol(sys.protocol);
      const c = card(sys.label);
      c.right.append(h('span', { class: 'badge', text: p.label }));
      c.body.append(kv(p.systemIdentity(sys.identity)));
      for (const site of sys.sites) {
        const live = site.id === liveId;
        const button = h('button', { class: 'btn', type: 'button', disabled: live, text: live ? 'Live' : 'Make live' });
        button.addEventListener('click', () => activate(site, button));
        c.body.append(h('div', { class: 'card-note' },
          h('div', { class: 'row' },
            h('strong', { text: site.label }), h('span', { class: 'dim', text: ` ${site.id}` }),
            h('div', { class: 'spacer' }), button),
          kv([['Control channel', mhz(site.control.freq_hz)], ...p.siteIdentity(site.identity)])));
      }
      list.append(c.el);
    }
  }

  api.systems().then(s => { systems = s; render(); }).catch(e => toast(e.message, true));

  return {
    update(s) {
      const live = s.status && s.status.live;
      const id = live && live.state === 'live' ? live.site.id : null;
      if (id !== liveId) {
        liveId = id;
        render();
      }
    },
    unmount() {},
  };
}
