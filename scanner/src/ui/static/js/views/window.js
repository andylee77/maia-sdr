// The receive window (Diagnostics): the slice of spectrum the live site is received in, against
// the traffic channels it grants and where the planner would put it, as a card; and the carriers
// heard in it over the last ten minutes, as a list for the page's main pane. The radio pushes
// both every few seconds while the page is open.

import { h, card, toast, table, setText } from '../dom.js';
import { ago, mhz, DASH } from '../format.js';
import { api } from '../api.js';
import { want } from '../store.js';

const share = (part, whole) => (whole > 0 ? `${Math.round((100 * part) / whole)} %` : DASH);

// `el`: the card; `heard`: the carriers' list; `update(store)` at each push; `stop()`.
export function windowCard() {
  const c = card('Receive window');
  const box = h('div');
  const heardTitle = h('p', { class: 'card-note' });
  const heardRows = h('div');
  const heard = h('div', { class: 'diag-pad' }, heardTitle, heardRows);
  const move = h('button', { class: 'btn small', type: 'button', text: "Move to the planner's window" });
  c.body.append(box,
    h('p', { class: 'card-note', text: 'The radio receives one slice of spectrum at a time; a call is followed only when its traffic channel is inside it. The planner places it over the busiest channels and moves it when both traffic channels are idle.' }));
  let site = null;
  let controlHz = null;

  function pushed(w) {
    if (!w.plan) {
      box.replaceChildren(h('p', { class: 'dim', text: 'No site is live.' }));
      setText(heardTitle, 'No site is live.');
      heardRows.replaceChildren();
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
    setText(heardTitle, s.carriers.length
      ? `${s.carriers.length} carrier${s.carriers.length === 1 ? '' : 's'} heard in the receive window${minutes >= 1 ? ` over the last ${Math.min(10, Math.round(minutes))} min` : ''}, the most active first.`
      : s.frames ? 'Nothing above the noise in the receive window yet.' : 'Listening.');
    heardRows.replaceChildren(...(s.carriers.length
      ? [table(['Frequency', 'On', 'Peak', ''], s.carriers.map(x => h('tr', null,
        h('td', { text: mhz(x.freq_hz) }), h('td', { text: `${x.on_pct < 1 ? x.on_pct.toFixed(1) : Math.round(x.on_pct)} %` }),
        h('td', { text: `+${Math.round(x.peak_db)} dB` }), h('td', { class: 'dim', text: what(x) }))))]
      : []));
  }

  return {
    el: c.el,
    heard,
    update(s) {
      const live = s.status && s.status.live;
      controlHz = live && live.state === 'live' ? live.site.control.freq_hz : null;
    },
    stop,
  };
}
