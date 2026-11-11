// The Scanner's diagnostics, filling the screen: the spectrum of the receive window across the
// top; under it the scanner's events (the control channel's messages and what the scanner did),
// with the carriers heard in the window behind the same tabs; and down the right the live site's
// control channel, its receive window and the tuning. The radio pushes each while the page is
// open. The hardware and the radio's own log are the rail's Diagnostics.

import { h } from '../dom.js';
import { mhz, ago, num, pct, DASH } from '../format.js';
import { protocol } from '../protocols.js';
import { spectrumCard } from './spectrum.js';
import { windowCard } from './window.js';
import { kvCard, mainPane, elsewhere } from './diag_parts.js';

// Kept while the page is left for another.
const state = { tab: 'events' };

export function mount(el) {
  const control = kvCard('Control channel');
  const tuning = kvCard('Tuning');
  const spectrum = spectrumCard();
  const receive = windowCard();
  const main = mainPane('scanner', state, {
    label: 'Events and carriers', panes: [['heard', 'Carriers', receive.heard]], housekeeping: true,
  });

  el.append(h('div', { class: 'diag' }, spectrum.el, main.el,
    h('div', { class: 'diag-side' }, control.el, receive.el, tuning.el,
      elsewhere('The hardware as the radio reads it back, and the radio\'s own log, are under ', h('a', { href: '#diag', text: 'Diagnostics' }), ' in the rail.'))));

  return {
    update(s) {
      receive.update(s);
      const st = s.status;
      if (!st) return;
      const t = st.tuning;
      const i = (st.control && st.control.input) || {};
      tuning.show([
        ['Preset', t.preset || DASH],
        ['LO (nominal)', mhz(t.lo_hz, 6)],
        ['Control channel', mhz(t.control_hz)],
        ['Lane 1', mhz(t.lanes[0])],
        ['Lane 2', mhz(t.lanes[1])],
        ['Control IQ', `${i.blocks ?? DASH} blocks (${i.dropped ?? 0} dropped, ${i.gaps ?? 0} after a gap)`],
      ]);
      // The live site's control channel: how well it decodes, by its protocol's counters.
      const live = st.live && st.live.state === 'live' ? st.live : null;
      const c = st.control || {};
      const p = live ? protocol(live.system.protocol) : null;
      control.show(live ? [
        ['Site', `${live.site.label} · ${mhz(live.site.control.freq_hz)}`],
        ['Messages', c.msgs_per_s === null || c.msgs_per_s === undefined ? DASH : `${num(c.msgs_per_s, 1)} /s`],
        [p.okLabel, pct(c.ok_pct)],
        ['Last message', ago(c.last_message_age_ms)],
        ...p.healthRows(c),
        ['Grants', num(c.grants)],
        ['Window', `${t.preset || DASH} at ${mhz(t.lo_hz)}`],
        ['CPU', pct(c.cpu_pct, 0)],
      ] : null, 'No site is live.');
    },
    unmount() {
      main.stop();
      spectrum.stop();
      receive.stop();
    },
  };
}
