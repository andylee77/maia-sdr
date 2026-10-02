// Systems: find the systems on the air; every configured system as a card (its name, details and
// identity, then its sites with their channels), edited in place; make a site live or stop it;
// remove a site or a system.

import { h, card, toast, iconButton } from '../dom.js';
import { api } from '../api.js';
import { refresh } from '../store.js';
import { scanCard } from './scan.js';
import { systemHead, siteTable, heardTraffic } from './cards.js';

export function mount(el) {
  const list = card('Systems');
  const scan = scanCard(load);
  el.append(h('div', { class: 'stack' }, scan.el, list.el));
  let liveId = null;
  let systems = [];
  // Learned state per site, for the traffic channels heard on the air.
  const learned = new Map();
  // A card with unsaved edits is not redrawn under the user.
  let editing = 0;

  async function act(button, call, done) {
    button.disabled = true;
    try {
      await call();
      toast(done);
      await refresh();
    } catch (e) {
      toast(e.message, true);
    } finally {
      button.disabled = false;
    }
  }

  async function remove(what, call) {
    if (!confirm(`Remove ${what}? What it learned goes too; the history keeps its calls.`)) return;
    try {
      await call();
      toast(`${what} removed`);
      load();
    } catch (e) {
      toast(e.message, true);
    }
  }

  // One configured system: head, sites, and Save once something changed.
  function systemCard(sys) {
    const box = h('div', { class: 'sys-card' });
    const bar = h('div', { class: 'savebar', hidden: true });
    let dirty = false;
    let headEdited = false;
    const touched = () => {
      if (!dirty) editing += 1;
      dirty = true;
      bar.hidden = false;
    };
    const hasLive = sys.sites.some(s => s.id === liveId);
    const drop = iconButton('trash', hasLive ? 'Delete the system (stop its live site first)' : 'Delete the system', {
      cls: 'danger', disabled: hasLive,
      onclick: () => remove(`${sys.label} with its ${sys.sites.length} site(s) and its aliases`, () => api.deleteSystem(sys.id)),
    });
    const head = systemHead(sys, { right: [drop], onInput: () => { headEdited = true; touched(); } });
    const table = siteTable(sys.protocol, sys.sites.map(site => {
      const live = site.id === liveId;
      const status = live
        ? [h('span', { class: 'badge ok', text: 'live' }), iconButton('stop', 'Stop', { onclick: e => act(e.currentTarget, () => api.stopSite(site.id), `${site.label} stopped`) })]
        : [iconButton('play', 'Make live', { onclick: e => act(e.currentTarget, () => api.activate(site.id), `${site.label} is live`) }),
          iconButton('trash', 'Delete the site', { cls: 'danger', onclick: () => remove(`site ${site.label}`, () => api.deleteSite(sys.id, site.id)) })];
      return { site, heard: heardTraffic(learned.get(site.id), site.control.freq_hz), tail: status };
    }), { traffic: true, settings: true, onInput: touched });

    const cancel = h('button', { class: 'btn small', type: 'button', text: 'Cancel', onclick: () => { editing -= 1; load(); } });
    const save = h('button', { class: 'btn small primary', type: 'button', text: 'Save' });
    save.addEventListener('click', async () => {
      try {
        const edits = table.rows.filter(r => r.edited()).map(r => [r.site, r.read()]);
        const sysEdit = headEdited ? head.read() : null;
        save.disabled = true;
        if (sysEdit) await api.saveSystem(sys.id, sysEdit);
        for (const [site, body] of edits) await api.saveSite(sys.id, site.id, body);
        toast(`${(sysEdit ? sysEdit.label : sys.label).trim()} saved`);
        editing -= 1;
        load();
      } catch (e) {
        toast(e.message, true);
        save.disabled = false;
      }
    });
    bar.append(cancel, save);
    box.append(head.el, table.el, bar);
    return box;
  }

  function render() {
    if (editing > 0) return;
    list.body.replaceChildren(...(systems.length
      ? systems.map(systemCard)
      : [h('p', { class: 'dim', text: 'None yet: scan for systems above.' })]));
  }

  async function load() {
    editing = 0;
    try {
      systems = await api.systems();
      await Promise.all(systems.flatMap(s => s.sites).map(site =>
        api.siteLearned(site.id).then(l => learned.set(site.id, l)).catch(() => {})));
      render();
    } catch (e) {
      toast(e.message, true);
    }
  }
  load();

  return {
    update(s) {
      const live = s.status && s.status.live;
      const id = live && live.state === 'live' ? live.site.id : null;
      if (id !== liveId) {
        liveId = id;
        render();
      }
    },
    unmount() { scan.stop(); },
  };
}
