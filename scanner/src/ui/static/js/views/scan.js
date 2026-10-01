// The scan card (Systems page): find the systems on the air, then tick and name what to add.
// Found sites are grouped by system identity; a site already configured is marked, and ticking
// it only adds the alternate control channels it announced.

import { h, card, toast } from '../dom.js';
import { mhz, num, pct } from '../format.js';
import { api } from '../api.js';
import { refresh } from '../store.js';
import { protocol } from '../protocols.js';

const POLL_MS = 1500;
const RUNNING = ['sweeping', 'probing', 'restoring'];

function hex(v, width) {
  return v === null || v === undefined ? '?' : v.toString(16).toUpperCase().padStart(width, '0');
}

// The system a found site belongs to, and default names for it and the site.
function systemOf(f) {
  const i = f.identity;
  return i.protocol === 'p25'
    ? { key: `p25:${i.wacn}-${i.system}`, label: `P25 WACN ${hex(i.wacn, 5)} system ${hex(i.system, 3)}` }
    : { key: `dmr:${i.model}-${i.network}`, label: `DMR ${i.model} network ${i.network}` };
}

function siteName(f) {
  const i = f.identity;
  return i.protocol === 'p25' ? `Site ${i.rfss}-${i.site}` : `Site ${i.site}`;
}

export function scanCard(onAdded) {
  const c = card('Find systems');
  let timer = null;
  let shown = null;

  async function start(button) {
    button.disabled = true;
    try {
      await api.scan();
      poll();
    } catch (e) {
      toast(e.message, true);
      button.disabled = false;
    }
  }

  function idle() {
    const button = h('button', { class: 'btn primary', type: 'button', text: 'Scan' });
    button.addEventListener('click', () => start(button));
    c.body.replaceChildren(
      h('p', { text: 'Look for P25 and DMR control channels on 700, 800 and 900 MHz, UHF and VHF. It takes about five minutes, and the live site pauses meanwhile.' }),
      h('div', { class: 'row' }, button));
  }

  function progress(s) {
    const cancel = h('button', { class: 'btn', type: 'button', text: 'Cancel' });
    cancel.addEventListener('click', () => api.scanCancel().catch(e => toast(e.message, true)));
    const where = s.probing_hz ? `listening to ${mhz(s.probing_hz)}` : s.state;
    c.body.replaceChildren(
      h('p', { text: `Window ${s.step} of ${s.steps}; ${num(s.probed)} of ${num(s.to_probe)} carriers probed; ${where}.` }),
      h('p', { class: 'dim', text: `${s.sites.length} control channels found so far.` }),
      h('div', { class: 'row' }, cancel));
  }

  function results(s) {
    const groups = new Map();
    for (const f of s.sites) {
      const sys = systemOf(f);
      if (!groups.has(sys.key)) groups.set(sys.key, { ...sys, sites: [] });
      groups.get(sys.key).sites.push(f);
    }
    const picks = [];
    const body = [];
    for (const g of groups.values()) {
      const known = g.sites.some(f => f.existing_site);
      const sysName = h('input', { class: 'input wide', type: 'text', value: g.label, 'aria-label': 'System name' });
      body.push(h('div', { class: 'row' }, h('strong', { text: protocol(g.sites[0].protocol).label }),
        known ? h('span', { class: 'dim', text: 'a configured system' }) : sysName));
      for (const f of g.sites) {
        const tick = h('input', { type: 'checkbox', 'aria-label': 'Add this site' });
        tick.checked = !f.existing_site;
        const name = h('input', { class: 'input wide', type: 'text', value: siteName(f), 'aria-label': 'Site name' });
        name.disabled = !!f.existing_site;
        picks.push(() => tick.checked && { key: f.id, label: name.value, system_label: known ? null : sysName.value });
        body.push(h('div', { class: 'row' }, tick, name,
          h('span', { text: mhz(f.freq_hz) }),
          h('span', { class: 'dim', text: `${num(f.msgs_per_s, 1)} msgs/s, ${pct(f.ok_pct)}${f.modulation ? ', ' + f.modulation.toUpperCase() : ''}${f.via_neighbour ? ', a neighbour' : ''}` }),
          f.existing_site ? h('span', { class: 'badge ok', text: `configured: ${f.existing_site}` }) : null));
      }
    }
    const add = h('button', { class: 'btn primary', type: 'button', text: 'Add ticked sites', disabled: !s.sites.length });
    add.addEventListener('click', async () => {
      const sites = picks.map(p => p()).filter(Boolean);
      if (!sites.length) return toast('Nothing ticked', true);
      add.disabled = true;
      try {
        const r = await api.scanAdd(sites);
        toast(`Added ${r.sites.length} sites${r.updated.length ? `; ${r.updated.length} known sites updated` : ''}`);
        await refresh();
        onAdded();
      } catch (e) {
        toast(e.message, true);
      } finally {
        add.disabled = false;
      }
    });
    const again = h('button', { class: 'btn', type: 'button', text: 'Scan again' });
    again.addEventListener('click', () => start(again));
    const summary = `${s.state === 'done' ? 'Done' : s.state === 'cancelled' ? 'Cancelled' : 'Failed'}: ${s.sites.length} control channels, `
      + `${s.traffic.length} traffic channels, ${s.other.length} other carriers.` + (s.error ? ` ${s.error}` : '');
    c.body.replaceChildren(h('p', { text: summary }), ...body,
      s.sites.length ? null : h('p', { class: 'dim', text: 'No control channels heard. A scan on the site antenna finds more.' }),
      h('div', { class: 'row' }, add, again));
  }

  async function poll() {
    clearTimeout(timer);
    let s;
    try {
      s = await api.scanState();
    } catch (e) {
      timer = setTimeout(poll, POLL_MS * 2);
      return;
    }
    if (RUNNING.includes(s.state)) {
      progress(s);
      timer = setTimeout(poll, POLL_MS);
    } else if (s.state === 'idle') {
      idle();
    } else if (shown !== `${s.id}:${s.state}`) {
      shown = `${s.id}:${s.state}`;
      results(s);
    }
  }

  poll();
  return { el: c.el, stop() { clearTimeout(timer); } };
}
