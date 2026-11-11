// Systems: on the left the scan for systems and the configured systems; on the right the one
// picked: a system (its name, details and identity, then its sites with their channels, edited
// in place; a site made live or stopped; a site or the system removed), or the scan (scan.js).

import { h, setText, setClass, toast, icon, iconButton } from '../dom.js';
import { api } from '../api.js';
import { refresh } from '../store.js';
import { protocol } from '../protocols.js';
import { scanPane, scanLine, scanRunning } from './scan.js';
import { systemHead, siteTable, heardTraffic } from './cards.js';

// The scan's place in the list.
const SCAN = 'scan';

export function mount(el) {
  const count = h('span', { class: 'dim' });
  const rows = h('div', { class: 'card-body' });
  const list = h('section', { class: 'card flush sys-list', 'aria-label': 'Systems' },
    h('div', { class: 'card-head' }, h('h2', { text: 'Systems' }), count), rows);
  const pane = h('section', { class: 'card sys-pane', 'aria-label': 'The picked system' });
  el.append(h('div', { class: 'systems' }, list, pane));

  const scan = scanPane(load);
  let liveId = null;
  let systems = null;
  let scanState = null;
  // The store's systems last read from.
  let seenSystems = null;
  // Learned state per site, for the traffic channels heard on the air.
  const learned = new Map();
  // A system with unsaved edits is not redrawn under the user.
  let editing = 0;
  // What the right side shows: a system's id or the scan; the live site's system until the user
  // picks one.
  let selected = null;
  let userPicked = false;
  // What the right side was drawn from, so it is drawn again only when that changed.
  let drawn = null;
  let reads = 0;

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
    const hasLive = sys.sites.some(site => site.id === liveId);
    const drop = iconButton('trash', hasLive ? 'Delete the system (stop its live site first)' : 'Delete the system', {
      cls: 'danger', disabled: hasLive,
      onclick: () => remove(`${sys.label} with its ${sys.sites.length} site(s) and its aliases`, () => api.deleteSystem(sys.id)),
    });
    const head = systemHead(sys, { right: [drop], onInput: () => { headEdited = true; touched(); } });
    const table = siteTable(sys.protocol, sys.sites.map(site => {
      const live = site.id === liveId;
      const status = live
        ? [h('span', { class: 'badge ok', text: 'live' }),
          h('button', { class: 'btn small', type: 'button', text: 'Stop', onclick: e => act(e.currentTarget, () => api.stopSite(site.id), `${site.label} stopped`) })]
        : [h('button', { class: 'btn small', type: 'button', text: 'Make live', onclick: e => act(e.currentTarget, () => api.activate(site.id), `${site.label} is live`) }),
          iconButton('trash', 'Delete the site', { cls: 'danger', onclick: () => remove(`site ${site.label}`, () => api.deleteSite(sys.id, site.id)) })];
      const l = learned.get(site.id);
      return {
        site, heard: heardTraffic(l, site.control.freq_hz), tail: status,
        lcn: l && l.lcn_hz, granted: l && l.lcns_granted, found: l && l.channels_heard,
      };
    }), { traffic: true, settings: true, onInput: touched });

    const cancel = h('button', { class: 'btn small', type: 'button', text: 'Cancel', onclick: () => load() });
    const save = h('button', { class: 'btn small primary', type: 'button', text: 'Save' });
    save.addEventListener('click', async () => {
      try {
        const edits = table.rows.filter(r => r.edited()).map(r => [r.site, r.read()]);
        const sysEdit = headEdited ? head.read() : null;
        save.disabled = true;
        if (sysEdit) await api.saveSystem(sys.id, sysEdit);
        for (const [site, body] of edits) await api.saveSite(sys.id, site.id, body);
        toast(`${(sysEdit ? sysEdit.label : sys.label).trim()} saved`);
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

  // A row of the list; `key` is what it picks.
  function pickRow(key, name, sub, mark) {
    const b = h('button', { class: 'sys-pick', type: 'button' }, name, mark, h('span', { class: 'sub', text: sub }));
    setClass(b, 'sel', key === selected);
    if (key === selected) b.setAttribute('aria-current', 'true');
    b.addEventListener('click', () => {
      if (key === selected) return;
      if (editing > 0 && !confirm('Leave this system without saving its changes?')) return;
      editing = 0;
      selected = key;
      userPicked = true;
      render();
    });
    return b;
  }

  // The list alone: it follows the scan and the live site while a system is being edited.
  function renderList() {
    setText(count, systems.length ? String(systems.length) : '');
    rows.replaceChildren(
      pickRow(SCAN, h('span', { class: 'name' }, icon('search'), 'Find systems'), scanLine(scanState),
        scanRunning(scanState) ? h('span', { class: 'badge acquiring', text: 'scanning' }) : null),
      ...systems.map(sys => {
        const live = sys.sites.find(site => site.id === liveId);
        const sites = `${sys.sites.length} site${sys.sites.length === 1 ? '' : 's'}`;
        return pickRow(sys.id, h('span', { class: 'name', text: sys.label }),
          `${protocol(sys.protocol).label} · ${live && sys.sites.length > 1 ? `${sites} · ${live.label}` : sites}`,
          live ? h('span', { class: 'badge ok', text: 'live' }) : null);
      }));
  }

  function render() {
    if (!systems) return;
    const live = systems.find(sys => sys.sites.some(site => site.id === liveId));
    if (!userPicked && live && editing === 0) selected = live.id;
    if (selected !== SCAN && !systems.some(sys => sys.id === selected)) selected = live ? live.id : systems.length ? systems[0].id : SCAN;
    renderList();
    if (editing > 0) return;
    // The scan draws itself; a system's card is drawn from what was read and the live site.
    const from = selected === SCAN ? SCAN : `${selected}|${liveId}|${reads}`;
    if (from === drawn) return;
    drawn = from;
    pane.replaceChildren(selected === SCAN ? scan.el : systemCard(systems.find(sys => sys.id === selected)));
  }

  async function load() {
    editing = 0;
    try {
      systems = await api.systems();
      await Promise.all(systems.flatMap(sys => sys.sites).map(site =>
        api.siteLearned(site.id).then(l => learned.set(site.id, l)).catch(() => {})));
      reads += 1;
      render();
    } catch (e) {
      toast(e.message, true);
    }
  }
  load();

  return {
    update(s) {
      scan.update(s);
      const live = s.status && s.status.live;
      const id = live && live.state === 'live' ? live.site.id : null;
      const changed = id !== liveId || s.scan !== scanState;
      liveId = id;
      scanState = s.scan;
      // The systems changed (here or from another page): read them again, unless one is being edited.
      if (s.systems && s.systems !== seenSystems) {
        const first = seenSystems === null;
        seenSystems = s.systems;
        if (!first && editing === 0) {
          load();
          return;
        }
      }
      if (changed) render();
    },
    unmount() {},
  };
}
