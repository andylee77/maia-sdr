// The scan card (Systems page): pick the bands and settings; follow the scan as /ws/live reports
// it (the band, the window, what it is listening to); then a card per found system to fill in
// and add. Found sites already configured are left out, and a card goes once its sites are
// added; a system already configured keeps its own name and details.

import { h, card, toast } from '../dom.js';
import { mhz, num, pct } from '../format.js';
import { api, wsUrl } from '../api.js';
import { refresh } from '../store.js';
import { protocol } from '../protocols.js';
import { systemHead, siteTable, inline } from './cards.js';

const RUNNING = ['sweeping', 'probing', 'restoring'];
const MHZ0 = hz => (hz / 1e6).toLocaleString(undefined, { maximumFractionDigits: 3 });
const range = ([low, high]) => `${MHZ0(low)}–${MHZ0(high)} MHz`;

// A found site's reception.
const reception = f => `${pct(f.ok_pct)} · ${num(f.msgs_per_s, 1)} msg/s${f.modulation ? ` · ${f.modulation.toUpperCase()}` : ''}${f.via_neighbour ? ' · from a neighbour' : ''}`;

// A scan's finds by system.
function groups(s) {
  const out = new Map();
  for (const f of s.sites) {
    const sys = protocol(f.protocol).scanSystem(f.identity);
    if (!out.has(sys.key)) out.set(sys.key, { ...sys, protocol: f.protocol, sites: [] });
    out.get(sys.key).sites.push(f);
  }
  return [...out.values()];
}

export function scanCard(onAdded) {
  const c = card('Find systems');
  let ws = null;
  let retry = null;
  let closed = false;
  // The scan whose cards are on screen (`id:state`): drawn once, so what is typed stays.
  let shown = null;
  let configured = [];
  let options = null;

  // The setup: bands, settings, and what they come to.
  function setup() {
    if (!options) {
      c.body.replaceChildren(h('p', { class: 'dim', text: "The radio did not say what it can scan; reload the page." }));
      return;
    }
    const d = options.defaults;
    const bands = options.bands.map(b => {
      const tick = h('input', { type: 'checkbox' });
      tick.checked = true;
      tick.addEventListener('change', sum);
      return { band: [b.low_hz, b.high_hz], tick, el: h('label', { class: 'chip' }, tick, h('strong', { text: b.name }), h('span', { class: 'dim', text: range([b.low_hz, b.high_hz]) })) };
    });
    const from = inline('', 'from', { min: 5, onInput: sum });
    const to = inline('', 'to', { min: 5, onInput: sum });
    const frames = inline(String(d.frames), 'frames', { min: 3 });
    const listen = inline(String(d.probe_ms / 1000), 's', { min: 3 });
    const identity = inline(String(d.identity_ms / 1000), 's', { min: 3 });
    const most = inline(String(d.max_candidates), 'carriers', { min: 3 });
    const note = h('p', { class: 'dim' });
    const go = h('button', { class: 'btn primary', type: 'button', text: 'Scan' });

    // The ticked bands and the typed one, in Hz.
    function chosen() {
      const out = bands.filter(b => b.tick.checked).map(b => b.band);
      const [a, b] = [Number(from.value), Number(to.value)];
      if (from.value.trim() || to.value.trim()) {
        if (!(a >= 70 && b <= 6000 && a < b)) throw new Error('Your range: from and to in MHz, 70 to 6000');
        out.push([Math.round(a * 1e6), Math.round(b * 1e6)]);
      }
      return out;
    }
    function sum() {
      let list = [];
      try {
        list = chosen();
      } catch {
        return;
      }
      const windows = list.reduce((n, [a, b]) => n + Math.max(1, Math.ceil((b - a) / options.step_hz)), 0);
      note.textContent = `The radio reads ${MHZ0(options.window_hz)} MHz at a time (${MHZ0(options.usable_hz)} MHz of it clean) and steps ${MHZ0(options.step_hz)} MHz: `
        + `${windows} window${windows === 1 ? '' : 's'} for these bands. The live site pauses meanwhile.`;
      go.disabled = !list.length;
    }
    go.addEventListener('click', async () => {
      try {
        const number = (el, what, lo, hi) => {
          const v = Number(el.value);
          if (!(v >= lo && v <= hi)) throw new Error(`${what}: ${lo} to ${hi}`);
          return v;
        };
        const req = {
          bands: chosen(),
          frames: number(frames, 'Frames per window', 1, 64),
          probe_ms: Math.round(number(listen, 'Listening to a carrier', 0.5, 30) * 1000),
          identity_ms: Math.round(number(identity, 'Waiting for a site', 0, 60) * 1000),
          max_candidates: number(most, 'Carriers', 1, 500),
        };
        go.disabled = true;
        await api.scan(req);
      } catch (e) {
        toast(e.message, true);
        go.disabled = false;
      }
    });
    sum();
    c.body.replaceChildren(h('div', { class: 'stack' },
      h('div', { class: 'chips' }, ...bands.map(b => b.el),
        h('span', { class: 'chip' }, h('span', { class: 'dim', text: 'and' }), from, h('span', { class: 'dim', text: 'to' }), to, h('span', { class: 'dim', text: 'MHz' }))),
      h('div', { class: 'scan-settings' },
        h('span', null, 'Read ', frames, ' spectrum frames a window'),
        h('span', null, 'listen ', listen, ' s to each carrier'),
        h('span', null, 'wait up to ', identity, " s for a site's identity"),
        h('span', null, 'try at most ', most, ' carriers')),
      note,
      h('div', { class: 'row' }, go)));
  }

  // The scan running: the band, the window, what it is doing, and what it found so far.
  function progress(s) {
    const cancel = h('button', { class: 'btn', type: 'button', text: 'Cancel' });
    cancel.addEventListener('click', () => api.scanCancel().catch(e => toast(e.message, true)));
    const named = s.band && options && options.bands.find(b => b.low_hz === s.band[0] && b.high_hz === s.band[1]);
    const where = s.band ? `${named ? `${named.name} · ` : ''}${range(s.band)} · window ${s.step} of ${s.steps}` : `Window ${s.step} of ${s.steps} done`;
    let doing;
    if (s.state === 'restoring') doing = 'Handing the radio back to the live site';
    else if (s.probing_hz && !s.band) doing = `Listening to ${mhz(s.probing_hz)}, a control channel a found site announced (${s.probed + 1} of ${s.to_probe})`;
    else if (s.probing_hz && s.state === 'probing') doing = `Listening to ${mhz(s.probing_hz)} for a control channel (carrier ${s.probed + 1} of ${s.to_probe})`;
    else doing = `Reading the spectrum around ${mhz(s.lo_hz)}`;
    const found = groups(s);
    c.body.replaceChildren(h('div', { class: 'stack' },
      h('div', null, h('strong', { text: where })),
      h('div', { class: 'dim', text: doing }),
      h('progress', { class: 'scan', max: String(Math.max(s.steps, 1)), value: String(Math.max(s.step - 1, 0)) }),
      ...(found.length ? [h('div', { class: 'dim', text: 'Found so far:' }), ...found.map(g => h('div', null,
        h('strong', { text: g.label }), ` (${protocol(g.protocol).label}): `,
        g.sites.map(f => `${protocol(f.protocol).scanSiteName(f.identity)} on ${mhz(f.freq_hz)}`).join(', ')))] : []),
      h('div', { class: 'row' }, cancel)));
  }

  // The configured system a found one is, by identity.
  function configuredAs(g) {
    const p = protocol(g.protocol);
    return configured.find(sys => sys.protocol === g.protocol && p.sameSystem(sys.identity || {}, g.sites[0].identity));
  }

  // The configured site a found one is: one on its control channel within 3 kHz (`moved`
  // false), or one it announces as an alternate or the same site of the same system, configured
  // on another channel (`moved` true).
  function configuredSite(f) {
    const p = protocol(f.protocol);
    const near = hz => Math.abs(hz - f.freq_hz) <= 3000;
    const key = p.siteKey(f.identity);
    for (const sys of configured.filter(s => s.protocol === f.protocol)) {
      for (const s of sys.sites) {
        if (near(s.control.freq_hz)) return { site: s, moved: false };
        const same = key && p.sameSystem(sys.identity || {}, f.identity) && p.siteKey(s.identity || {}) === key;
        if (same || (s.control.alternates_hz || []).some(near)) return { site: s, moved: true };
      }
    }
    return null;
  }
  // Something to add: a new site, or a configured one heard on another channel.
  const toAdd = f => { const c = configuredSite(f); return !c || c.moved; };

  // The found cards on screen, to drop the ones with nothing left to add.
  let cards = [];
  let cardList = null;

  function prune() {
    cards = cards.filter(x => {
      if (x.sites.some(toAdd)) return true;
      x.el.remove();
      return false;
    });
    if (cardList && !cards.length) cardList.replaceChildren(h('p', { class: 'dim', text: 'Every system found is added.' }));
  }

  // A found system's card: its head (a configured one's name), its new sites ticked, one Add.
  function foundCard(g) {
    const p = protocol(g.protocol);
    const known = configuredAs(g);
    const box = h('div', { class: 'sys-card' });
    const add = h('button', { class: 'btn primary small', type: 'button', text: known ? 'Add sites' : 'Add system' });
    const head = known ? null : systemHead({ protocol: g.protocol, label: g.label, identity: p.heardSystem(g.sites[0].identity), details: {} }, { right: [add] });
    const fresh = g.sites.filter(f => !configuredSite(f));
    const tick = label => {
      const t = h('input', { type: 'checkbox', 'aria-label': label });
      t.checked = true;
      return t;
    };
    const ticks = fresh.map(() => tick('Add this site'));
    const moves = g.sites.map(f => ({ f, c: configuredSite(f) })).filter(x => x.c && x.c.moved).map(x => ({
      ...x,
      tick: tick('Move it to the channel it was heard on'),
    }));
    const table = siteTable(g.protocol, fresh.map((f, i) => ({
      site: {
        label: p.scanSiteName(f.identity), identity: p.heardSite(f.identity),
        control: { freq_hz: f.freq_hz, alternates_hz: (f.secondary_hz || []).filter(x => Math.abs(x - f.freq_hz) > 3000), timeslot: f.timeslot },
        channels_hz: [],
      },
      lead: ticks[i],
      tail: [h('span', { class: 'dim', text: reception(f) })],
    })), { traffic: false });
    add.addEventListener('click', async () => {
      try {
        const sites = [
          ...fresh.map((f, i) => ticks[i].checked && { key: f.id, ...table.rows[i].read() }),
          ...moves.map(m => m.tick.checked && { key: m.f.id, label: m.c.site.label }),
        ].filter(Boolean);
        if (!sites.length) throw new Error('Tick a site to add');
        const sys = known ? { label: known.label } : head.read();
        add.disabled = true;
        const r = await api.scanAdd({ ...sys, sites });
        toast(`${sys.label.trim()}: ${r.sites.length} site${r.sites.length === 1 ? '' : 's'} added`);
        await loadConfigured();
        prune();
        await refresh();
        onAdded();
      } catch (e) {
        toast(e.message, true);
        add.disabled = false;
      }
    });
    if (known && !fresh.length) add.textContent = 'Update';
    box.append(
      head ? head.el : h('div', { class: 'row' }, h('span', { class: 'sys-title', text: known.label }), h('span', { class: 'badge', text: p.label }),
        h('span', { class: 'dim', text: 'configured' }), h('span', { class: 'spacer' }), add),
      ...(fresh.length ? [table.el] : []),
      ...moves.map(m => h('label', { class: 'row moved' }, m.tick,
        h('span', null, h('strong', { text: m.c.site.label }), ` is configured on ${mhz(m.c.site.control.freq_hz)} but heard on ${mhz(m.f.freq_hz)} (${reception(m.f)}): `,
          h('span', { class: 'dim', text: 'ticked, it moves there.' })))));
    return { el: box, sites: g.sites };
  }

  function results(s) {
    const again = h('button', { class: 'btn', type: 'button', text: 'New scan' });
    again.addEventListener('click', setup);
    const summary = `${s.state === 'done' ? 'Found' : s.state === 'cancelled' ? 'Cancelled; found' : 'Failed; found'} ${s.sites.length} control channel${s.sites.length === 1 ? '' : 's'}`
      + ` (and ${s.traffic.length} traffic channels, ${s.other.length} other carriers) in ${s.bands.map(range).join(', ')}.` + (s.error ? ` ${s.error}` : '');
    cards = groups(s).filter(g => g.sites.some(toAdd)).map(foundCard);
    cardList = h('div', { class: 'stack' }, ...cards.map(x => x.el));
    if (s.sites.length && !cards.length) cardList.append(h('p', { class: 'dim', text: 'Every system found is added.' }));
    const none = s.sites.length ? [] : [h('p', { class: 'dim', text: 'No control channels heard. A scan on the site antenna finds more.' })];
    c.body.replaceChildren(h('div', { class: 'stack' }, h('p', { class: 'dim', text: summary }), cardList, ...none, h('div', { class: 'row' }, again)));
  }

  function show(s) {
    if (RUNNING.includes(s.state)) {
      shown = null;
      progress(s);
    } else if (s.state === 'idle') {
      if (shown !== 'idle') setup();
      shown = 'idle';
    } else if (shown !== `${s.id}:${s.state}`) {
      shown = `${s.id}:${s.state}`;
      results(s);
    }
  }

  async function loadConfigured() {
    try {
      configured = await api.systems();
    } catch { /* every find shows as new */ }
  }

  function connect(backoff = 1000) {
    if (closed) return;
    ws = new WebSocket(wsUrl('/ws/live'));
    ws.onopen = () => { backoff = 1000; };
    ws.onmessage = e => {
      const m = JSON.parse(e.data);
      if (m.type === 'snapshot' || m.type === 'scan') show(m.scan);
      else if (m.type === 'changed' && m.what === 'systems') loadConfigured().then(prune);
    };
    ws.onclose = () => {
      if (!closed) retry = setTimeout(() => connect(Math.min(backoff * 2, 30000)), backoff);
    };
  }

  Promise.all([loadConfigured(), api.scanOptions().then(o => { options = o; })])
    .catch(e => toast(e.message, true))
    .finally(() => connect());
  return {
    el: c.el,
    stop() {
      closed = true;
      clearTimeout(retry);
      if (ws) ws.close();
    },
  };
}
