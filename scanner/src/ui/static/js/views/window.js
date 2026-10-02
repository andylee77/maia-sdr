// The receive window card (Diagnostics): the slice of spectrum the live site is received in,
// against the traffic channels it grants, where the planner would put it, and the carriers heard
// in it over the last ten minutes; the radio pushes them every few seconds while the card is open.

import { h, card, toast, table } from '../dom.js';
import { ago, mhz, DASH } from '../format.js';
import { api } from '../api.js';
import { want } from '../store.js';

const share = (part, whole) => (whole > 0 ? `${Math.round((100 * part) / whole)} %` : DASH);

export function windowCard() {
  const c = card('Receive window');
  const box = h('div');
  const heard = h('div');
  const move = h('button', { class: 'btn small', type: 'button', text: "Move to the planner's window" });
  c.body.append(
    h('p', { class: 'dim', text: 'The radio receives one slice of spectrum at a time; a call is followed only when its traffic channel is inside it. The planner places it over the busiest channels and moves it when both traffic channels are idle.' }),
    box, heard);
  let site = null;
  let controlHz = null;

  function pushed(w) {
    if (!w.plan) {
      box.replaceChildren(h('p', { class: 'dim', text: 'No site is live.' }));
      heard.replaceChildren();
      return;
    }
    site = w.plan.site;
    show(w.plan);
    showSurvey(w.survey);
  }
  const stop = want({ window: true }, m => pushed(m.window));

  move.addEventListener('click', async () => {
    move.disabled = true;
    try {
      const r = await api.recentre(site);
      toast(r.moved_to ? `Window moved: ${r.moved_to.preset} at ${mhz(r.moved_to.lo_hz)}` : "The window is the planner's already");
      show(await api.sitePlan(site));
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

  // The carriers heard, the most active first: steady ones are control channels and the like,
  // intermittent ones carry calls, data or keep-alives.
  function showSurvey(s) {
    const minutes = s.frames / 7.6 / 60;
    const what = x => {
      if (controlHz && Math.abs(x.freq_hz - controlHz) <= 3000) return 'this control channel';
      return x.steady ? 'steady: a control channel?' : 'intermittent: calls or data';
    };
    heard.replaceChildren(
      h('h3', { text: `Heard in the window${minutes >= 1 ? `, last ${Math.min(10, Math.round(minutes))} min` : ''}` }),
      s.carriers.length
        ? table(['Frequency', 'On', 'Peak', ''], s.carriers.map(x => h('tr', null,
          h('td', { text: mhz(x.freq_hz) }), h('td', { text: `${x.on_pct < 1 ? x.on_pct.toFixed(1) : Math.round(x.on_pct)} %` }),
          h('td', { text: `+${Math.round(x.peak_db)} dB` }), h('td', { class: 'dim', text: what(x) }))))
        : h('p', { class: 'dim', text: s.frames ? 'Nothing above the noise yet.' : 'Listening.' }));
  }

  return {
    el: c.el,
    update(s) {
      const live = s.status && s.status.live;
      controlHz = live && live.state === 'live' ? live.site.control.freq_hz : null;
    },
    stop,
  };
}
