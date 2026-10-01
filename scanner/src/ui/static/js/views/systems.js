// Systems: find the systems on the air; every configured system and its sites; make a site live;
// remove a site or a system.

import { h, card, toast } from '../dom.js';
import { ago, mhz, DASH } from '../format.js';
import { api } from '../api.js';
import { refresh } from '../store.js';
import { protocol } from '../protocols.js';
import { scanCard } from './scan.js';

const MHZ = hz => (hz / 1e6).toFixed(5).replace(/0+$/, '').replace(/\.$/, '');

// "454.36875, 451.0875" <-> Hz.
function hzList(text, what) {
  return text.split(/[\s,;]+/).filter(Boolean).map(t => {
    const v = Math.round(Number(t) * 1e6);
    if (!Number.isFinite(v) || v < 70e6 || v > 6e9) throw new Error(`${what}: ${t} is not a frequency in MHz`);
    return v;
  });
}

// "5 = 454.36875" lines <-> {5: Hz}.
function lcnPlan(text) {
  const out = {};
  for (const line of text.split('\n').map(l => l.trim()).filter(Boolean)) {
    const m = line.match(/^(\d+)\s*[=:,]\s*([\d.]+)$/);
    if (!m) throw new Error(`"${line}" is not "LCN = MHz"`);
    out[m[1]] = hzList(m[2], `LCN ${m[1]}`)[0];
  }
  return out;
}

// The site editor: everything but the site's id, identity and origin.
function siteEditor(sys, site, done) {
  const p = protocol(sys.protocol);
  const input = (value, label, cls = 'input wide') => h('input', { class: cls, type: 'text', value, 'aria-label': label });
  const label = input(site.label, 'Name');
  const control = input(MHZ(site.control.freq_hz), 'Control channel, MHz', 'input');
  const alternates = input((site.control.alternates_hz || []).map(MHZ).join(', '), 'Alternate control channels, MHz');
  const channels = input((site.channels_hz || []).map(MHZ).join(', '), 'Known channels, MHz');
  const modulation = h('select', { class: 'input', 'aria-label': 'Modulation' },
    ...[['auto', 'Automatic'], ['lsm', 'LSM'], ['c4fm', 'C4FM']].map(([v, t]) => h('option', { value: v, text: t })));
  modulation.value = site.modulation || 'auto';
  const lcn = input(site.control.lcn ?? '', 'Control LCN', 'input num');
  const slot = h('select', { class: 'input', 'aria-label': 'Control timeslot' }, h('option', { value: '', text: '?' }),
    h('option', { value: '1', text: 'TS1' }), h('option', { value: '2', text: 'TS2' }));
  slot.value = site.control.timeslot ? String(site.control.timeslot) : '';
  const plan = h('textarea', { class: 'input', rows: 4, 'aria-label': 'LCN plan' });
  plan.value = Object.entries((site.channel_plan && site.channel_plan.lcn_hz) || {}).map(([k, v]) => `${k} = ${MHZ(v)}`).join('\n');
  const auto = h('input', { type: 'checkbox' });
  auto.checked = site.window ? site.window.auto : true;
  const position = h('select', { class: 'input', 'aria-label': 'Control channel in the window' },
    ...[['center', 'Centre'], ['top', 'Top (traffic below)'], ['bottom', 'Bottom (traffic above)']].map(([v, t]) => h('option', { value: v, text: t })));
  position.value = (site.window && site.window.cc_position) || 'center';
  const save = h('button', { class: 'btn primary', type: 'button', text: 'Save site' });
  const cancel = h('button', { class: 'btn', type: 'button', text: 'Cancel', onclick: () => done(false) });
  save.addEventListener('click', async () => {
    try {
      const body = {
        label: label.value,
        control: {
          freq_hz: hzList(control.value, 'control channel')[0],
          alternates_hz: hzList(alternates.value, 'alternates'),
          lcn: p.edits.controlSlot && lcn.value !== '' ? Number(lcn.value) : null,
          timeslot: p.edits.controlSlot && slot.value ? Number(slot.value) : null,
        },
        modulation: p.edits.modulation ? modulation.value : site.modulation,
        channels_hz: hzList(channels.value, 'channels'),
        channel_plan: p.edits.lcnPlan ? { lcn_hz: lcnPlan(plan.value) } : site.channel_plan,
        window: { ...(site.window || {}), auto: auto.checked, cc_position: position.value },
        notes: site.notes || [],
      };
      save.disabled = true;
      await api.saveSite(sys.id, site.id, body);
      toast(`${body.label} saved`);
      done(true);
    } catch (e) {
      toast(e.message, true);
      save.disabled = false;
    }
  });
  const row = (text, ...nodes) => h('div', { class: 'row' }, h('span', { text }), ...nodes);
  return h('div', { class: 'stack' },
    row('Name', label),
    row('Control channel (MHz)', control, ...(p.edits.controlSlot ? [h('span', { class: 'dim', text: 'LCN' }), lcn, slot] : [])),
    row('Alternates (MHz)', alternates),
    ...(p.edits.modulation ? [row('Modulation', modulation)] : []),
    ...(p.edits.lcnPlan ? [h('label', { class: 'stack' }, h('span', { text: 'Channels: one "LCN = MHz" a line' }), plan)] : []),
    row('Known channels (MHz)', channels),
    h('label', { class: 'row' }, auto, h('span', { text: 'Move the window to the busiest channels' })),
    row('Control channel in the window', position),
    h('div', { class: 'row end' }, cancel, save));
}

function kv(rows) {
  return h('table', { class: 'kv' }, ...rows.map(([k, v]) =>
    h('tr', null, h('th', { text: k }), h('td', { text: v === null || v === undefined ? DASH : v }))));
}

// What the site announces of its neighbours and other channels, appended to `into` once read.
function announced(site, into) {
  api.siteLearned(site.id).then(l => {
    const rows = [];
    if (l.neighbours.length) {
      rows.push(['Neighbours', l.neighbours.map(n => `${n.system.toString(16).toUpperCase().padStart(3, '0')}-${n.rfss}-${n.site}${n.control_hz ? ` (${mhz(n.control_hz)})` : ''}`).join(', ')]);
    }
    if (l.secondary_control_hz.length) rows.push(['Other control channels', l.secondary_control_hz.map(f => mhz(f)).join(', ')]);
    if (l.data_channel_hz) rows.push(['Data channel', mhz(l.data_channel_hz)]);
    if (rows.length) into.append(kv(rows));
  }).catch(() => {});
}

const share = (part, whole) => (whole > 0 ? `${Math.round((100 * part) / whole)} %` : DASH);

// The live site's receive window against its channels, and the planner's choice.
function windowBox(site) {
  const box = h('div');
  const move = h('button', { class: 'btn small', type: 'button', text: "Move to the planner's window" });
  const load = () => api.sitePlan(site.id).then(show).catch(() => box.replaceChildren());
  move.addEventListener('click', async () => {
    move.disabled = true;
    try {
      const r = await api.recentre(site.id);
      toast(r.moved_to ? `Window moved: ${r.moved_to.preset} at ${mhz(r.moved_to.lo_hz)}` : "The window is the planner's already");
      load();
    } catch (e) {
      toast(e.message, true);
    } finally {
      move.disabled = false;
    }
  });
  function show(v) {
    const inside = v.channels.filter(c => c.covered).length;
    const outside = v.channels.filter(c => !c.covered && c.weight > 0).map(c => mhz(c.freq_hz));
    const rows = [
      ['Window', `${v.preset || DASH} at ${mhz(v.lo_hz)}: ${inside} of ${v.channels.length} channels, ${share(v.covered_weight, v.total_weight)} of the grants`],
      ['Planner', v.best ? `${v.best.preset} at ${mhz(v.best.lo_hz)}: ${share(v.best.covered_weight, v.best.total_weight)}${v.better ? ', worth a move' : ''}` : DASH],
      ['Moves', `${v.auto ? 'automatic' : 'by hand only'}${v.last_recentre_unix_ms ? `, last ${ago(Date.now() - v.last_recentre_unix_ms)}` : ''}`],
    ];
    if (outside.length) rows.push(['Outside it', outside.join(', ')]);
    box.replaceChildren(kv(rows), h('div', { class: 'row end' }, move));
  }
  load();
  return box;
}

export function mount(el) {
  const list = h('div', { class: 'stack' });
  const scan = scanCard(load);
  el.append(h('div', { class: 'stack' }, scan.el, list));
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

  function render() {
    list.replaceChildren();
    if (!systems.length) {
      list.append(card('No systems yet').el);
      return;
    }
    for (const sys of systems) {
      const p = protocol(sys.protocol);
      const c = card(sys.label);
      const hasLive = sys.sites.some(s => s.id === liveId);
      const drop = h('button', {
        class: 'btn small danger', type: 'button', text: 'Delete system', disabled: hasLive,
        title: hasLive ? 'Make a site of another system live first' : '',
        onclick: () => remove(`${sys.label} with its ${sys.sites.length} site(s) and its profiles`, () => api.deleteSystem(sys.id)),
      });
      c.right.append(h('span', { class: 'badge', text: p.label }), drop);
      c.body.append(kv(p.systemIdentity(sys.identity)));
      for (const site of sys.sites) {
        const live = site.id === liveId;
        const button = h('button', { class: 'btn', type: 'button', disabled: live, text: live ? 'Live' : 'Make live' });
        button.addEventListener('click', () => activate(site, button));
        const details = h('div');
        const show = () => {
          details.replaceChildren(kv([['Control channel', mhz(site.control.freq_hz)], ...p.siteIdentity(site.identity)]));
          announced(site, details);
        };
        const edit = h('button', { class: 'btn small', type: 'button', text: 'Edit' });
        const del = h('button', {
          class: 'btn small danger', type: 'button', text: 'Delete', disabled: live,
          title: live ? 'Make another site live first' : '',
          onclick: () => remove(`site ${site.label}`, () => api.deleteSite(sys.id, site.id)),
        });
        edit.addEventListener('click', () => details.replaceChildren(siteEditor(sys, site, saved => (saved ? load() : show()))));
        show();
        c.body.append(h('div', { class: 'card-note' },
          h('div', { class: 'row' },
            h('strong', { text: site.label }), h('span', { class: 'dim', text: ` ${site.id}` }),
            h('div', { class: 'spacer' }), edit, del, button),
          details,
          ...(live ? [windowBox(site)] : [])));
      }
      list.append(c.el);
    }
  }

  function load() {
    api.systems().then(s => { systems = s; render(); }).catch(e => toast(e.message, true));
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
