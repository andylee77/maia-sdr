// The receive window card (Diagnostics): the slice of spectrum the live site is received in,
// against the traffic channels it grants, and where the planner would put it.

import { h, card, toast } from '../dom.js';
import { ago, mhz, DASH } from '../format.js';
import { api } from '../api.js';

const share = (part, whole) => (whole > 0 ? `${Math.round((100 * part) / whole)} %` : DASH);
// The plan moves slowly: grants add up over minutes.
const REFRESH_MS = 15000;

export function windowCard() {
  const c = card('Receive window');
  const box = h('div');
  const move = h('button', { class: 'btn small', type: 'button', text: "Move to the planner's window" });
  c.body.append(h('p', { class: 'dim', text: 'The radio receives one slice of spectrum at a time; a call is followed only when its traffic channel is inside it. The planner places it over the busiest channels and moves it when both traffic channels are idle.' }), box);
  let site = null;
  let timer = null;

  async function load() {
    if (!site) {
      box.replaceChildren(h('p', { class: 'dim', text: 'No site is live.' }));
      return;
    }
    try {
      show(await api.sitePlan(site));
    } catch {
      box.replaceChildren();
    }
  }

  move.addEventListener('click', async () => {
    move.disabled = true;
    try {
      const r = await api.recentre(site);
      toast(r.moved_to ? `Window moved: ${r.moved_to.preset} at ${mhz(r.moved_to.lo_hz)}` : "The window is the planner's already");
      load();
    } catch (e) {
      toast(e.message, true);
    } finally {
      move.disabled = false;
    }
  });

  function show(v) {
    const inside = v.channels.filter(x => x.covered).length;
    const outside = v.channels.filter(x => !x.covered && x.weight > 0).map(x => mhz(x.freq_hz));
    const rows = [
      ['Window', `${v.preset || DASH} at ${mhz(v.lo_hz)}: ${inside} of ${v.channels.length} channels, ${share(v.covered_weight, v.total_weight)} of the grants`],
      ['Planner', v.best ? `${v.best.preset} at ${mhz(v.best.lo_hz)}: ${share(v.best.covered_weight, v.best.total_weight)}${v.better ? ', worth a move' : ''}` : DASH],
      ['Moves', `${v.auto ? 'automatic' : 'by hand only'}${v.last_recentre_unix_ms ? `, last ${ago(Date.now() - v.last_recentre_unix_ms)}` : ''}`],
    ];
    if (outside.length) rows.push(['Outside it', outside.join(', ')]);
    box.replaceChildren(h('dl', { class: 'kv' }, ...rows.flatMap(([k, val]) => [h('dt', { text: k }), h('dd', { text: val })])),
      h('div', { class: 'row end' }, move));
  }

  return {
    el: c.el,
    update(s) {
      const live = s.status && s.status.live;
      const id = live && live.state === 'live' ? live.site.id : null;
      if (id !== site) {
        site = id;
        clearInterval(timer);
        timer = site ? setInterval(load, REFRESH_MS) : null;
        load();
      }
    },
    stop() { clearInterval(timer); },
  };
}
