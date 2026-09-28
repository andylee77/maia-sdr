// Change 070: "Coverage" (Radio page). The receive window against the
// site's channels: the site file's list plus every frequency granted
// here, busiest drawn tallest; a listed channel never granted once the
// plan is learned (1000 grants) is grey and does not count. A grant
// outside the window is not
// followed ("outside the window" in Recent calls). "Recentre" moves the
// preset and LO to the planner's best window; with "Automatically" on
// the radio does that by itself when both chains are idle (at most every
// 10 minutes, only for a real gain).

import { h, setText, card, toast, switchInput } from '../dom.js';
import { api } from '../api.js';
import { kick } from '../store.js';
import { mhz, ago, DASH } from '../format.js';
import { kvTable } from './kv_table.js';

const SVG = 'http://www.w3.org/2000/svg';
const svg = (tag, attrs) => {
  const el = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs || {})) el.setAttribute(k, String(v));
  return el;
};

export function coverageCard() {
  const c = card('Coverage');
  const kv = kvTable();
  const chart = svg('svg', { viewBox: '0 0 1000 64', preserveAspectRatio: 'none', role: 'img', 'aria-label': 'Receive window and channels' });
  Object.assign(chart.style, { width: '100%', height: '64px', display: 'block', marginTop: '10px' });
  const missed = h('p', { class: 'card-note' });
  const best = h('p', { class: 'card-note' });
  const recentre = h('button', { class: 'btn', type: 'button', text: 'Recentre' });
  const auto = switchInput('Automatically when idle', true, on => setPlan({ auto: on }));
  // Narrowest window the planner may pick for this site.
  const minSel = h('select', { class: 'input', 'aria-label': 'Narrowest window' },
    h('option', { value: '', text: 'Narrowest window: fit' }),
    ...['8M', '12M', '16M'].map(p => h('option', { value: p, text: 'Narrowest window: ' + p })));
  minSel.addEventListener('change', () => setPlan({ min_preset: minSel.value || null }));
  c.body.append(kv.el, chart, missed, best,
    h('div', { class: 'row', style: { marginTop: '10px', flexWrap: 'wrap', gap: '8px' } }, recentre, minSel, h('div', { class: 'spacer' }), auto.el));

  let plan = null;
  let busy = false;

  async function setPlan(patch) {
    busy = true;
    try {
      const r = await api.putSitePlan(patch);
      if (r.save_error) toast('Applied, but NOT saved: ' + r.save_error, true);
    } catch (e) {
      toast('Not changed: ' + e.message, true);
    }
    busy = false;
    load();
  }

  recentre.addEventListener('click', async () => {
    const b = plan && plan.best;
    if (!b) return;
    if (!confirm('Move the receive window to ' + b.preset + ', LO ' + mhz(b.lo_hz, 4) + '?\nA few seconds without decode; a call on the air is cut.')) return;
    recentre.disabled = true;
    try {
      await api.recentre();
      toast('Recentred: ' + b.preset + ', LO ' + mhz(b.lo_hz, 4));
      kick(500);
    } catch (e) {
      toast('Recentre failed: ' + e.message, true);
    }
    recentre.disabled = false;
    setTimeout(load, 1500);
  });

  function draw(p) {
    const fs = p.channels.map(x => x.freq_hz).concat([p.low_hz, p.high_hz, p.control_hz]);
    if (p.best) fs.push(p.best.lo_hz - p.best.usable_half_hz, p.best.lo_hz + p.best.usable_half_hz);
    const lo = Math.min(...fs) - 150e3;
    const hi = Math.max(...fs) + 150e3;
    const x = f => ((f - lo) / (hi - lo)) * 1000;
    const maxG = Math.max(1, ...p.channels.map(ch => ch.grants));
    const nodes = [];
    if (p.best && p.better) {
      nodes.push(svg('rect', { x: x(p.best.lo_hz - p.best.usable_half_hz), y: 2, width: x(p.best.lo_hz + p.best.usable_half_hz) - x(p.best.lo_hz - p.best.usable_half_hz), height: 60, fill: 'none', stroke: 'var(--accent)', 'stroke-dasharray': '6 4', 'vector-effect': 'non-scaling-stroke' }));
    }
    nodes.push(svg('rect', { x: x(p.low_hz), y: 6, width: x(p.high_hz) - x(p.low_hz), height: 52, fill: 'color-mix(in srgb, var(--accent) 16%, transparent)' }));
    for (const ch of p.channels) {
      const hgt = 10 + 40 * Math.log1p(ch.grants) / Math.log1p(maxG);
      const fill = ch.weight <= 0 ? 'var(--text-dim)' : ch.covered ? 'var(--accent)' : 'var(--danger)';
      const t = svg('rect', { x: x(ch.freq_hz) - 2, y: 58 - hgt, width: 4, height: hgt, fill });
      const tip = svg('title');
      tip.textContent = mhz(ch.freq_hz, 5) + ' · ' + ch.grants + ' grants' + (ch.listed ? '' : ' · learned') + (ch.weight <= 0 ? ' · never granted' : '') + (ch.covered ? '' : ' · outside');
      t.append(tip);
      nodes.push(t);
    }
    nodes.push(svg('rect', { x: x(p.control_hz) - 1, y: 0, width: 2, height: 64, fill: 'var(--text)' }));
    chart.replaceChildren(...nodes);
  }

  function render(p) {
    plan = p;
    const live = p.channels.filter(ch => ch.weight > 0);
    const n = live.length;
    const inside = live.filter(ch => ch.covered).length;
    const pct = p.total_weight > 0 ? Math.round((100 * p.covered_weight) / p.total_weight) : 100;
    kv.set([
      ['Window', mhz(p.low_hz, 3) + ' – ' + mhz(p.high_hz, 3) + ' (' + p.preset + ')'],
      ['Channels', inside + ' of ' + n + ' inside · ' + pct + '% of the traffic'],
      ['Last recentre', p.last_recentre_unix_ms ? ago(Date.now() - p.last_recentre_unix_ms) : 'never'],
    ]);
    draw(p);
    const out = p.channels.filter(ch => !ch.covered && ch.weight > 0).sort((a, b) => b.grants - a.grants);
    const unused = p.channels.filter(ch => ch.weight <= 0).length;
    setText(missed, (out.length
      ? 'Outside: ' + out.slice(0, 8).map(ch => (ch.freq_hz / 1e6).toFixed(5) + (ch.grants ? ' (' + ch.grants + ')' : '')).join(', ') + (out.length > 8 ? ' and ' + (out.length - 8) + ' more' : '')
      : 'Every channel in use is inside the window.')
      + (unused ? ' ' + unused + ' listed channel' + (unused > 1 ? 's' : '') + ' never granted (grey) do not count.' : ''));
    if (p.best && p.better) {
      setText(best, 'Better: ' + p.best.preset + ', LO ' + mhz(p.best.lo_hz, 4) + ' covers ' + Math.round((100 * p.best.covered_weight) / Math.max(p.best.total_weight, 1)) + '% (dashed).' + (p.locked ? ' The LO is locked (Control channel card).' : ''));
    } else {
      setText(best, 'This is the best window for the known channels.');
    }
    recentre.disabled = !(p.best && p.better) || p.locked;
    if (!busy && document.activeElement !== auto.input) auto.input.checked = !!p.auto;
    if (!busy && document.activeElement !== minSel) minSel.value = p.min_preset || '';
    c.el.hidden = false;
  }

  async function load() {
    try {
      const r = await api.sitePlan();
      render(r.plan);
    } catch {
      c.el.hidden = true;
    }
  }

  c.el.hidden = true;
  load();
  return { el: c.el, load };
}
