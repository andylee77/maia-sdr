// The ATSC page (ATSC TV mode): the TV channel finder. Pick the bands and settings, follow the scan
// as /ws/live reports it, then every channel read: what it holds (8-VSB by its pilot, a signal
// without the 8-VSB pilot, or nothing), its carrier to noise, its pilot and its power.

import { h, card, toast, table } from '../dom.js';
import { mhz, num, DASH } from '../format.js';
import { api } from '../api.js';
import { inline } from './cards.js';

const KIND = {
  '8vsb': { label: '8-VSB', cls: 'ok', title: 'ATSC 1.0: its pilot is there' },
  no_pilot: { label: 'No 8-VSB pilot', cls: 'alert', title: 'A signal fills the channel without the 8-VSB pilot: ATSC 3.0, or something else' },
  vacant: { label: 'Vacant', cls: '', title: 'Nothing above the noise' },
};

const db = v => (v === null || v === undefined ? DASH : `${num(v, 1)} dB`);
const signed = v => (v > 0 ? '+' : v < 0 ? '−' : '') + num(Math.abs(v) / 1000, 1);

export function mount(el) {
  const c = card('TV channels');
  el.append(h('div', { class: 'stack' }, c.el));
  let options = null;
  let last = null;
  // What is on screen: the setup, a running scan, or one scan's results (`id:state`), drawn once
  // so what is typed stays.
  let shown = null;
  // The scan whose results were put away for a new setup.
  let dismissed = null;

  function bands() {
    const out = new Map();
    for (const ch of options.channels.filter(x => x.reachable)) {
      if (!out.has(ch.band)) out.set(ch.band, []);
      out.get(ch.band).push(ch);
    }
    return [...out.entries()].map(([name, chans]) => ({ name, chans }));
  }

  function setup() {
    shown = 'setup';
    if (!options) {
      c.body.replaceChildren(h('p', { class: 'dim', text: 'Reading the channel plan…' }));
      return;
    }
    const d = options.defaults;
    const picks = bands().map(b => {
      const tick = h('input', { type: 'checkbox' });
      tick.checked = true;
      tick.addEventListener('change', sum);
      const first = b.chans[0];
      const end = b.chans[b.chans.length - 1];
      const span = `RF ${first.number}–${end.number} · ${mhz(first.low_hz, 0)} to ${mhz(end.low_hz + 6e6, 0)}`;
      return { b, tick, el: h('label', { class: 'chip' }, tick, h('strong', { text: b.name }), h('span', { class: 'dim', text: span })) };
    });
    const frames = inline(String(d.frames), 'frames', { min: 3 });
    const gain = inline(d.gain_db == null ? '' : String(d.gain_db), 'AGC', { min: 4 });
    const note = h('p', { class: 'dim' });
    const go = h('button', { class: 'btn primary', type: 'button', text: 'Scan' });

    const chosen = () => picks.filter(p => p.tick.checked).flatMap(p => p.b.chans.map(ch => ch.number));
    // Two adjacent channels a window, as the radio reads them.
    function windows(list) {
      let n = 0;
      for (let i = 0; i < list.length; i++, n++) {
        const a = options.channels.find(x => x.number === list[i]);
        const b = options.channels.find(x => x.number === list[i + 1]);
        if (a && b && b.low_hz === a.low_hz + 6e6) i++;
      }
      return n;
    }
    function sum() {
      const list = chosen();
      const w = windows(list);
      note.textContent = `${list.length} channels in ${w} window${w === 1 ? '' : 's'} of ${num(options.window_hz / 1e6)} MHz, two channels a window. `
        + 'Each channel shows its pilot (8-VSB), its carrier to noise and its power.';
      go.disabled = !list.length;
    }
    go.addEventListener('click', async () => {
      try {
        const n = Number(frames.value);
        if (!(n >= 1 && n <= 64)) throw new Error('Frames per window: 1 to 64');
        const g = gain.value.trim();
        if (g && !(Number(g) >= -3 && Number(g) <= 76)) throw new Error('Gain: -3 to 76 dB, or empty for the AGC');
        go.disabled = true;
        await api.atscScan({ channels: chosen(), frames: n, gain_db: g ? Math.round(Number(g)) : null });
      } catch (e) {
        toast(e.message, true);
        go.disabled = false;
      }
    });
    sum();
    c.body.replaceChildren(h('div', { class: 'stack' },
      h('div', { class: 'chips' }, ...picks.map(p => p.el)),
      h('div', { class: 'scan-settings' },
        h('span', null, 'Read ', frames, ' spectrum frames a window'),
        h('span', null, 'gain ', gain, ' dB (empty: the AGC)')),
      note,
      h('div', { class: 'row' }, go)));
  }

  function progress(s) {
    shown = 'running';
    const cancel = h('button', { class: 'btn', type: 'button', text: 'Cancel' });
    cancel.addEventListener('click', () => api.atscCancel().catch(e => toast(e.message, true)));
    const found = s.found.filter(x => x.kind !== 'vacant');
    c.body.replaceChildren(h('div', { class: 'stack' },
      h('div', null, h('strong', { text: `Window ${s.step} of ${s.steps}` }), s.lo_hz ? h('span', { class: 'dim', text: ` · around ${mhz(s.lo_hz, 1)}` }) : null),
      h('progress', { class: 'scan', max: String(Math.max(s.steps, 1)), value: String(Math.max(s.step - 1, 0)) }),
      found.length ? h('div', { class: 'dim', text: `Found so far: ${found.map(x => `RF ${x.number}${x.kind === 'no_pilot' ? ' (no 8-VSB pilot)' : ''}`).join(', ')}` }) : null,
      h('div', { class: 'row' }, cancel)));
  }

  function row(ch) {
    const k = KIND[ch.kind] || KIND.vacant;
    const what = h('td', null, h('span', { class: `badge ${k.cls}`.trim(), text: k.label, title: k.title }),
      ch.clipped ? h('span', { class: 'badge bad', text: 'Overload', title: 'The ADC clipped while this window was read: try a lower manual gain' }) : null);
    const vacant = ch.kind === 'vacant';
    return h('tr', { class: vacant ? 'faint' : null },
      h('td', null, h('strong', { text: String(ch.number) })),
      h('td', { class: 'num', text: mhz(ch.center_hz, 0) }),
      what,
      h('td', { class: 'num', text: db(ch.level_db), title: 'Carrier to noise: the plateau over the noise floor at the channel edges' }),
      h('td', { class: 'num', text: db(ch.pilot_db), title: 'The pilot over the plateau: 20.1 dB for a clean signal' }),
      h('td', { class: 'num', text: ch.pilot_offset_hz == null ? DASH : `${signed(ch.pilot_offset_hz)} kHz` }),
      h('td', { class: 'num', text: ch.power_dbm == null ? DASH : `${num(ch.power_dbm, 1)} dBm` }),
      h('td', { class: 'num dim', text: ch.gain_db == null ? DASH : `${num(ch.gain_db)} dB` }));
  }

  function results(s) {
    shown = `${s.id}:${s.state}`;
    const again = h('button', { class: 'btn', type: 'button', text: 'New scan' });
    again.addEventListener('click', () => {
      dismissed = s.id;
      setup();
    });
    const count = kind => s.found.filter(x => x.kind === kind).length;
    const head = { done: 'Done', cancelled: 'Cancelled', error: 'Failed' }[s.state] || s.state;
    const tally = `${count('8vsb')} with 8-VSB, ${count('no_pilot')} filled without the 8-VSB pilot (ATSC 3.0 or other), `
      + `${count('vacant')} vacant, of ${s.found.length} channels read${s.gain_db == null ? ' with the AGC' : ` at ${s.gain_db} dB of gain`}.`;
    const summary = s.found.length ? `${head}: ${tally}${s.error ? ` ${s.error}.` : ''}` : `${head}${s.error ? `: ${s.error}` : ''}.`;
    c.body.replaceChildren(h('div', { class: 'stack' },
      h('p', { class: 'dim', text: summary }),
      s.found.length ? table(['RF', 'Centre', 'Holds', 'C/N', 'Pilot', 'Pilot offset', 'Power', 'Gain'], s.found.map(row)) : null,
      h('div', { class: 'row' }, again)));
  }

  function draw(s) {
    if (!s || !options) return;
    if (s.state === 'sweeping') progress(s);
    else if (s.state === 'idle' || s.id === dismissed) {
      if (shown !== 'setup') setup();
    } else if (shown !== `${s.id}:${s.state}`) results(s);
  }

  setup();
  api.atscOptions()
    .then(o => {
      options = o;
      shown = null;
      if (last && last.state !== 'idle') draw(last);
      else setup();
    })
    .catch(e => toast(e.message, true));

  return {
    update(store) {
      if (store.atsc && store.atsc !== last) {
        last = store.atsc;
        draw(last);
      }
    },
    unmount() {},
  };
}
