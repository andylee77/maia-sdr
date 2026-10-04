// The ATSC page (ATSC TV mode): the TV channel finder. Pick the bands and settings, then one table of
// the channels asked for, filled in as /ws/live reports the scan: each channel as its window is read
// (what it holds: 8-VSB by its pilot, a signal without the 8-VSB pilot, or nothing; its carrier to
// noise, its pilot and its power), then its station's own names as its channel is decoded. A
// channel picked in the table shows its spectrum and its station's virtual channels.

import { h, card, toast, table, setClass } from '../dom.js';
import { mhz, num, DASH } from '../format.js';
import { api } from '../api.js';
import { inline } from './cards.js';

const KIND = {
  '8vsb': { label: '8-VSB', cls: 'ok', title: 'ATSC 1.0: its pilot is there' },
  no_pilot: { label: 'No 8-VSB pilot', cls: 'alert', title: 'A signal fills the channel without the 8-VSB pilot: ATSC 3.0, or something else' },
  vacant: { label: 'Vacant', cls: '', title: 'Nothing above the noise' },
};

// The AGC lets a few peaks clip; above this many samples a million at the ADC's full scale the
// radio is overloaded.
const OVERLOAD_PPM = 1000;
const clipText = ppm => (ppm == null ? '' : `${num(ppm, ppm < 10 ? 1 : 0)} samples a million at the ADC's full scale`);

const db = v => (v === null || v === undefined ? DASH : `${num(v, 1)} dB`);
const SERVICE = { 2: 'TV', 3: 'Audio', 4: 'Data' };
const vchan = c => `${c.major}.${c.minor} ${c.short_name}`;

// A decoded station in a table cell: its main channels (x.1) first, then how many more.
function stationText(st) {
  if (!st) return { text: DASH, title: 'Not decoded (not 8-VSB, or under 15 dB)' };
  if (st.error) return { text: 'not decoded', title: st.error };
  if (st.channels.length) {
    const main = st.channels.filter(c => c.minor === 1);
    const shown = (main.length ? main : st.channels.slice(0, 1)).map(vchan);
    const more = st.channels.length - shown.length;
    return { text: shown.join(', ') + (more > 0 ? ` +${more}` : ''), title: st.channels.map(vchan).join(', ') };
  }
  if (st.tsid != null) return { text: `TSID ${st.tsid}, no channel table`, title: 'Decoded, but no virtual channel table in the capture' };
  return { text: 'too weak to decode', title: `${st.failed} of ${st.packets} packets failed${st.mer_db == null ? '' : `; MER ${num(st.mer_db, 1)} dB`}` };
}

// A decoded station: how it decoded, then its virtual channels.
function stationDetails(st) {
  if (!st) return [];
  if (st.error) return [h('p', { class: 'dim', text: `Not decoded: ${st.error}.` })];
  const clock = st.time_unix ? `; its clock ${new Date(st.time_unix * 1000).toISOString().replace('T', ' ').slice(0, 19)} UTC` : '';
  const how = `${st.tsid == null ? 'No transport stream id' : `TSID ${st.tsid}`}; MER ${st.mer_db == null ? DASH : `${num(st.mer_db, 1)} dB`}; `
    + `${num(st.packets)} packets, ${num(st.failed)} beyond Reed-Solomon${clock}.`;
  const out = [h('p', { class: 'dim', text: how })];
  if (st.channels.length) {
    out.push(table(['Channel', 'Name', 'Long name', 'Program', 'Service', ''], st.channels.map(c => h('tr', null,
      h('td', { class: 'num', text: `${c.major}.${c.minor}` }),
      h('td', { text: c.short_name }),
      h('td', { class: 'dim', text: c.long_name || DASH }),
      h('td', { class: 'num', text: String(c.program) }),
      h('td', { text: SERVICE[c.service_type] || `type ${c.service_type}` }),
      h('td', { class: 'dim', text: [c.hidden ? 'hidden' : '', c.access_controlled ? 'scrambled' : ''].filter(Boolean).join(', ') })))));
  }
  return out;
}
const signed = v => (v > 0 ? '+' : v < 0 ? '−' : '') + num(Math.abs(v) / 1000, 1);

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || '#888';
}

// One channel's spectrum: the channel shaded, the plan's pilot and the window's LO marked.
function drawChannel(canvas, sp) {
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth || 600;
  const ht = canvas.clientHeight || 220;
  canvas.width = Math.round(w * dpr);
  canvas.height = Math.round(ht * dpr);
  const g = canvas.getContext('2d');
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, w, ht);
  const sorted = [...sp.db].sort((a, b) => a - b);
  const lo = sorted[Math.floor(sorted.length * 0.02)] - 3;
  const hi = sorted[sorted.length - 1] + 3;
  const left = 44, bottom = 18, right = 6;
  const ph = ht - bottom;
  const span = sp.db.length * sp.bin_hz;
  const x = hz => left + ((hz - sp.start_hz) / span) * (w - left - right);
  const y = v => ph - ((v - lo) / (hi - lo)) * ph;
  g.fillStyle = cssVar('--accent');
  g.globalAlpha = 0.08;
  g.fillRect(x(sp.low_hz), 0, x(sp.high_hz) - x(sp.low_hz), ph);
  g.globalAlpha = 0.6;
  g.fillRect(x(sp.pilot_hz) - 0.5, 0, 1, ph);
  g.fillStyle = cssVar('--text-faint');
  g.globalAlpha = 0.5;
  if (sp.lo_hz > sp.start_hz && sp.lo_hz < sp.start_hz + span) g.fillRect(x(sp.lo_hz) - 0.5, 0, 1, ph);
  g.globalAlpha = 1;
  g.strokeStyle = cssVar('--text-dim');
  g.lineWidth = 1;
  g.beginPath();
  sp.db.forEach((v, i) => {
    const px = x(sp.start_hz + i * sp.bin_hz);
    if (i === 0) g.moveTo(px, y(v));
    else g.lineTo(px, y(v));
  });
  g.stroke();
  g.fillStyle = cssVar('--text-faint');
  g.font = '11px system-ui, sans-serif';
  g.textBaseline = 'top';
  g.textAlign = 'center';
  for (const f of [sp.low_hz, (sp.low_hz + sp.high_hz) / 2, sp.high_hz]) g.fillText(`${(f / 1e6).toFixed(1)} MHz`, x(f), ph + 4);
  g.textAlign = 'right';
  for (const v of [hi - 3, (hi + lo) / 2, lo + 3]) {
    g.textBaseline = 'middle';
    g.fillText(`${v.toFixed(0)}`, left - 6, y(v));
  }
}

export function mount(el) {
  const c = card('TV channels');
  el.append(h('div', { class: 'stack' }, c.el));
  let options = null;
  let last = null;
  // What is on screen: the setup (drawn once, so what is typed stays) or a scan's table.
  let shown = null;
  // The scan on screen: its id and how to bring it up to date.
  let view = null;
  // The scan whose results were put away for a new setup.
  let dismissed = null;
  // The channel whose spectrum is shown, and its drawing again at a new width.
  let picked = null;
  let redraw = null;
  const onResize = () => redraw && redraw();
  window.addEventListener('resize', onResize);

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
    view = null;
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
    const name = h('input', { type: 'checkbox' });
    name.checked = d.identify;
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
        await api.atscScan({ channels: chosen(), frames: n, gain_db: g ? Math.round(Number(g)) : null, identify: name.checked });
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
        h('span', null, 'gain ', gain, ' dB (empty: the AGC)'),
        h('label', { class: 'row', title: 'Each 8-VSB channel of 15 dB or more is tuned on its own and decoded: a few seconds each' }, name, 'name the stations')),
      note,
      h('div', { class: 'row' }, go)));
  }

  const HEAD = ['RF', 'Centre', 'Holds', 'Station', 'C/N', 'Pilot', 'Pilot offset', 'Power', 'Gain'];

  // A channel read: what it holds and how well; its station, or "naming" while it is decoded.
  function row(ch, naming) {
    const k = KIND[ch.kind] || KIND.vacant;
    const over = ch.clips_ppm != null && ch.clips_ppm >= OVERLOAD_PPM;
    const what = h('td', null, h('span', { class: `badge ${k.cls}`.trim(), text: k.label, title: k.title }),
      over ? h('span', { class: 'badge bad', text: 'Overload', title: `${num(ch.clips_ppm)} samples a million at the ADC's full scale: try a lower manual gain` }) : null);
    const vacant = ch.kind === 'vacant';
    const st = stationText(ch.station);
    const station = naming
      ? h('td', null, h('span', { class: 'badge acquiring', text: 'Naming…', title: 'Tuned on its own and being decoded' }))
      : h('td', { text: st.text, title: st.title });
    return h('tr', { class: vacant ? 'faint' : null },
      h('td', null, h('strong', { text: String(ch.number) })),
      h('td', { class: 'num', text: mhz(ch.center_hz, 0) }),
      what,
      station,
      h('td', { class: 'num', text: db(ch.level_db), title: 'Carrier to noise: the plateau over the noise floor at the channel edges' }),
      h('td', { class: 'num', text: db(ch.pilot_db), title: 'The pilot over the plateau: 20.1 dB for a clean signal' }),
      h('td', { class: 'num', text: ch.pilot_offset_hz == null ? DASH : `${signed(ch.pilot_offset_hz)} kHz` }),
      h('td', { class: 'num', text: ch.power_dbm == null ? DASH : `${num(ch.power_dbm, 1)} dBm` }),
      h('td', { class: 'num dim', text: ch.gain_db == null ? DASH : `${num(ch.gain_db)} dB`, title: clipText(ch.clips_ppm) }));
  }

  const plan = n => options.channels.find(x => x.number === n);

  // A channel not read yet; "reading" while its window is.
  function pending(n, reading) {
    const p = plan(n);
    return h('tr', { class: 'faint' },
      h('td', null, h('strong', { text: String(n) })),
      h('td', { class: 'num', text: p ? mhz(p.low_hz + 3e6, 0) : DASH }),
      h('td', null, reading ? h('span', { class: 'badge acquiring', text: 'Reading…' }) : DASH),
      h('td', { text: DASH }),
      ...['', '', '', '', ''].map(() => h('td', { class: 'num', text: DASH })));
  }

  // Whether channel n lies in the window around `lo_hz`.
  function inWindow(n, lo) {
    const p = plan(n);
    return !!p && Math.abs(p.low_hz + 3e6 - lo) < options.window_hz / 2;
  }

  // Above the table: where the scan is, with Cancel; once over, its tally, with New scan.
  function status(s) {
    if (s.state === 'sweeping') {
      const cancel = h('button', { class: 'btn', type: 'button', text: 'Cancel' });
      cancel.addEventListener('click', () => api.atscCancel().catch(e => toast(e.message, true)));
      const naming = s.identifying != null;
      return [
        naming
          ? h('div', null, h('strong', { text: `Naming the stations: RF ${s.identifying}` }), h('span', { class: 'dim', text: ` · ${s.identified + 1} of ${s.to_identify}` }))
          : h('div', null, h('strong', { text: `Window ${s.step} of ${s.steps}` }), s.lo_hz ? h('span', { class: 'dim', text: ` · around ${mhz(s.lo_hz, 1)}` }) : null),
        naming
          ? h('progress', { class: 'scan', max: String(Math.max(s.to_identify, 1)), value: String(s.identified) })
          : h('progress', { class: 'scan', max: String(Math.max(s.steps, 1)), value: String(Math.max(s.step - 1, 0)) }),
        h('div', { class: 'row' }, cancel)];
    }
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
    return [h('p', { class: 'dim', text: summary }), h('div', { class: 'row' }, again)];
  }

  // One scan's table: a row for each channel asked for, kept and replaced in place as the scan
  // reports it, so a picked channel and its spectrum stay while the rest fills in. Once the scan
  // is over, only the channels read are left.
  function scanView(first) {
    shown = 'scan';
    picked = null;
    redraw = null;
    const top = h('div', { class: 'stack' });
    const wrap = table(HEAD, []);
    const body = wrap.querySelector('tbody');
    const title = h('div', { class: 'dim' });
    const canvas = h('canvas', { style: { width: '100%', height: '220px', display: 'block' }, 'aria-label': "The channel's spectrum" });
    const about = h('div', { class: 'stack' });
    const plot = h('div', { class: 'stack', hidden: true }, title, canvas, about);
    const asked = first.channels.length ? first.channels : first.found.map(x => x.number);
    const rows = new Map();
    let now = first;
    let pickedNumber = null;

    async function pick(n) {
      pickedNumber = n;
      for (const [k, r] of rows) setClass(r.tr, 'sel', k === n);
      try {
        picked = await api.atscChannel(n);
        plot.hidden = false;
        title.textContent = `RF ${n}, ${mhz(picked.low_hz, 0)} to ${mhz(picked.high_hz, 0)} (shaded), as its window read it: `
          + "dB a bin, about dBm; the 8-VSB pilot's place and the window's LO marked.";
        drawChannel(canvas, picked);
        redraw = () => drawChannel(canvas, picked);
        const ch = now.found.find(x => x.number === n);
        about.replaceChildren(...stationDetails(ch && ch.station));
      } catch (e) {
        toast(e.message, true);
      }
    }

    function update(s) {
      now = s;
      const running = s.state === 'sweeping';
      const found = new Map(s.found.map(x => [x.number, x]));
      const list = [...asked, ...s.found.map(x => x.number).filter(n => !asked.includes(n))]
        .filter(n => running || found.has(n));
      for (const [n, r] of rows) {
        if (!list.includes(n)) {
          r.tr.remove();
          rows.delete(n);
        }
      }
      let after = null;
      for (const n of list) {
        const ch = found.get(n);
        const naming = running && s.identifying === n;
        const reading = running && !ch && s.lo_hz != null && inWindow(n, s.lo_hz);
        const sig = JSON.stringify([ch || null, naming, reading]);
        let r = rows.get(n);
        if (!r || r.sig !== sig) {
          const tr = ch ? row(ch, naming) : pending(n, reading);
          if (ch) {
            tr.classList.add('pick');
            tr.title = 'Show its spectrum';
            tr.addEventListener('click', () => pick(n));
          }
          setClass(tr, 'sel', n === pickedNumber);
          if (r) r.tr.replaceWith(tr);
          else if (after) after.after(tr);
          else body.prepend(tr);
          r = { tr, sig };
          rows.set(n, r);
          if (n === pickedNumber && ch && !plot.hidden) about.replaceChildren(...stationDetails(ch.station));
        }
        after = r.tr;
      }
      wrap.hidden = !list.length;
      top.replaceChildren(...status(s));
    }

    c.body.replaceChildren(h('div', { class: 'stack' }, top, wrap, plot));
    return { id: first.id, update };
  }

  function draw(s) {
    if (!s || !options) return;
    if (s.state === 'idle' || s.id === dismissed) {
      if (shown !== 'setup') setup();
      return;
    }
    if (!view || view.id !== s.id) view = scanView(s);
    view.update(s);
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
    unmount() {
      window.removeEventListener('resize', onResize);
    },
  };
}
