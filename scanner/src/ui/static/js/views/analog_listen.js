// The Analog module's Listen page: the unit as an analog receiver. The bands in a strip, with
// Listen (this browser's sound); the tuner under it: the frequency with a step either way and an
// entry of its own, how the signal is demodulated (wide FM, narrow FM, AM), the squelch, and a
// meter of what is on the frequency. The radio pushes what it is tuned to and what its meter
// reads; a tuning made here is the radio's, so another page shows it too. It listens only while
// it has the radio: tuned meanwhile, it listens there next.

import { h, toast, setText, setClass, icon } from '../dom.js';
import { api } from '../api.js';
import { want } from '../store.js';
import { analogPlayer } from '../audio/analog.js';

const KINDS = [['wfm', 'WFM', 'Wide FM: broadcast radio'], ['nfm', 'NFM', 'Narrow FM: two-way radio, amateur repeaters, weather radio'], ['am', 'AM', 'AM: aircraft']];
const KIND_WORDS = { wfm: 'wide FM', nfm: 'narrow FM', am: 'AM' };
// The squelch the page turns on with, dB over the floor.
const SQUELCH_DB = 8;
// The meter's full scale, dB over the floor.
const METER_DB = 40;
const mhz4 = hz => (hz / 1e6).toFixed(4);

function stateLine(v) {
  if (v.state === 'error') return `Not listening: ${v.error}`;
  if (v.state === 'starting') return 'Starting…';
  if (v.state !== 'listening') return 'Not listening: another module has the radio. A frequency set now is listened to once this one has it.';
  const w = v.window;
  const how = w.source === 'replay' ? 'a replayed capture in the radio\'s place' : 'the gain the radio\'s own';
  return `Listening: a ${w.sample_rate_hz / 1e6} MSPS window at ${(w.lo_hz / 1e6).toFixed(2)} MHz, ${how}.`;
}

export function mount(el) {
  let view = null;
  let gone = false;
  // The frequency last listened to in each band, so its chip comes back to it.
  const last = new Map();

  // ── The bands, and this browser's sound ──
  const chips = h('div', { class: 'data-wins analog-bands' });
  const playState = h('span', { class: 'data-state', 'aria-live': 'polite' });
  const playBtn = h('button', { class: 'btn', type: 'button', 'aria-pressed': 'false', title: 'Hear it in this browser' }, icon('speaker', 16), h('span', { text: 'Listen' }));
  const strip = h('section', { class: 'card data-strip' }, chips, h('div', { class: 'data-keep' }, playState, playBtn));
  playBtn.addEventListener('click', () => {
    if (analogPlayer.playing) analogPlayer.stop();
    else analogPlayer.start();
  });
  const unplay = analogPlayer.onChange(st => {
    setText(playBtn.lastChild, st.playing ? 'Stop' : 'Listen');
    playBtn.setAttribute('aria-pressed', String(st.playing));
    setClass(playBtn, 'lit', st.playing);
    setText(playState, st.label);
    playState.title = st.detail;
  });
  const resume = () => analogPlayer.resume();
  for (const kind of ['pointerdown', 'keydown']) document.addEventListener(kind, resume, { capture: true });

  // ── The tuner ──
  const down = h('button', { class: 'btn icon', type: 'button', 'aria-label': 'A step down' }, icon('back', 18));
  const up = h('button', { class: 'btn icon analog-up', type: 'button', 'aria-label': 'A step up' }, icon('back', 18));
  const readout = h('output', { class: 'analog-mhz' });
  const kindWord = h('span', { class: 'analog-kind dim' });
  const entry = h('input', { class: 'input num', type: 'number', step: 'any', inputmode: 'decimal', 'aria-label': 'Frequency in MHz' });
  const go = h('button', { class: 'btn', type: 'submit', text: 'Tune' });
  const form = h('form', { class: 'analog-entry' }, h('span', { class: 'dim', text: 'Frequency' }), entry, h('span', { class: 'dim', text: 'MHz' }), go);
  const kinds = KINDS.map(([id, label, title]) => h('button', { type: 'button', text: label, title, 'aria-pressed': 'false', dataset: { kind: id } }));
  const kindSeg = h('div', { class: 'seg', role: 'group', 'aria-label': 'How the signal is demodulated' }, ...kinds);
  const squelchOn = h('input', { type: 'checkbox', 'aria-label': 'Squelch' });
  const squelch = h('input', { class: 'analog-squelch', type: 'range', min: 0, max: METER_DB, step: 1, 'aria-label': 'Squelch, dB over the floor' });
  const squelchSays = h('span', { class: 'dim num analog-squelch-says' });
  const fill = h('i', { class: 'analog-fill' });
  const mark = h('i', { class: 'analog-mark' });
  const meter = h('div', { class: 'analog-meter', role: 'img' }, fill, mark);
  const meterSays = h('span', { class: 'analog-meter-says num', 'aria-live': 'polite' });
  const state = h('p', { class: 'card-note analog-state' });
  const tuner = h('section', { class: 'card analog-tuner' },
    h('div', { class: 'analog-freq' }, down, readout, h('span', { class: 'analog-unit dim', text: 'MHz' }), up, kindWord),
    h('div', { class: 'analog-controls' }, form, kindSeg,
      h('label', { class: 'analog-squelch-row' }, squelchOn, h('span', { class: 'dim', text: 'Squelch' }), squelch, squelchSays)),
    h('div', { class: 'analog-level' }, meter, meterSays),
    state,
    h('p', { class: 'card-note', text: 'One frequency at a time, heard while this module has the radio. Not built yet: memories, a scan of a band, recording, stereo. The radio tunes 70 MHz to 6 GHz: the amateur bands under 70 MHz are out of its reach.' }));

  const band = hz => (view ? view.bands.find(b => hz >= b.low_hz && hz <= b.high_hz) : null);
  // The step either way: the band's own, or 25 kHz (200 kHz for wide FM) outside every band.
  const stepOf = t => (band(t.freq_hz) ? band(t.freq_hz).step_hz : t.demod === 'wfm' ? 200000 : 25000);

  async function tune(to) {
    try {
      view = await api.analogTune(to);
      if (!gone) draw();
    } catch (e) {
      toast(e.message, true);
    }
  }
  // A frequency set by hand or by a step keeps the kind and the squelch, but a band entered
  // from outside it takes the band's kind.
  function tuneTo(hz) {
    const t = view.tuned;
    const into = band(hz);
    const demod = into && into !== band(t.freq_hz) ? into.demod : t.demod;
    tune({ freq_hz: hz, demod, squelch_db: demod === 'wfm' ? null : demod === t.demod ? t.squelch_db : SQUELCH_DB });
  }
  function step(way) {
    if (!view) return;
    const t = view.tuned;
    const by = stepOf(t);
    // On the band's raster, counted from its lower edge.
    const b = band(t.freq_hz);
    const base = b ? b.low_hz : 0;
    const next = base + (Math.round((t.freq_hz - base) / by) + way) * by;
    tuneTo(Math.max(view.low_hz, Math.min(view.high_hz, next)));
  }
  down.addEventListener('click', () => step(-1));
  up.addEventListener('click', () => step(1));
  form.addEventListener('submit', e => {
    e.preventDefault();
    const hz = Math.round(Number(entry.value) * 1e6);
    if (!view || !Number.isFinite(hz) || hz <= 0) return toast('A frequency in MHz, as 102.9.', true);
    return tuneTo(hz);
  });
  for (const b of kinds) {
    b.addEventListener('click', () => {
      if (!view) return;
      const t = view.tuned;
      tune({ freq_hz: t.freq_hz, demod: b.dataset.kind, squelch_db: b.dataset.kind === 'wfm' ? null : t.squelch_db ?? SQUELCH_DB });
    });
  }
  const setSquelch = () => {
    if (!view) return;
    const t = view.tuned;
    tune({ freq_hz: t.freq_hz, demod: t.demod, squelch_db: squelchOn.checked ? Number(squelch.value) : null });
  };
  squelchOn.addEventListener('change', setSquelch);
  squelch.addEventListener('change', setSquelch);
  squelch.addEventListener('input', () => setText(squelchSays, `${squelch.value} dB`));
  // The arrow keys step while the tuner has the focus, but not inside its own controls.
  tuner.addEventListener('keydown', e => {
    if (e.target.matches('input')) return;
    if (e.key === 'ArrowLeft') step(-1);
    else if (e.key === 'ArrowRight') step(1);
  });

  el.append(h('div', { class: 'data-page analog-page' }, strip, tuner));

  function chip(b) {
    const c = h('button', { class: 'data-win', type: 'button', title: `${mhz4(b.low_hz)} to ${mhz4(b.high_hz)} MHz, ${KIND_WORDS[b.demod]}`, dataset: { band: b.id } }, h('strong', { text: b.label }));
    c.addEventListener('click', () => {
      if (!view || band(view.tuned.freq_hz) === b) return;
      tune({ freq_hz: last.get(b.id) ?? b.low_hz, demod: b.demod, squelch_db: b.demod === 'wfm' ? null : SQUELCH_DB });
    });
    return c;
  }

  function draw() {
    if (!view) return;
    const t = view.tuned;
    if (!chips.children.length) chips.append(...view.bands.map(chip));
    const here = band(t.freq_hz);
    if (here) last.set(here.id, t.freq_hz);
    for (const c of chips.children) setClass(c, 'on', !!here && c.dataset.band === here.id);
    setText(readout, mhz4(t.freq_hz));
    setText(kindWord, `${KIND_WORDS[t.demod]}${here ? ` · ${here.label}` : ''}`);
    if (document.activeElement !== entry) entry.value = String(t.freq_hz / 1e6);
    for (const b of kinds) {
      setClass(b, 'on', b.dataset.kind === t.demod);
      b.setAttribute('aria-pressed', String(b.dataset.kind === t.demod));
    }
    const narrow = t.demod !== 'wfm';
    squelchOn.disabled = !narrow;
    squelchOn.checked = narrow && t.squelch_db !== null;
    squelch.disabled = !narrow || t.squelch_db === null;
    if (document.activeElement !== squelch) squelch.value = String(t.squelch_db ?? SQUELCH_DB);
    setText(squelchSays, !narrow ? 'wide FM has none' : t.squelch_db === null ? 'off' : `${t.squelch_db} dB`);
    // The meter: how far the carrier is over the floor beside it, with the squelch's mark; for
    // wide FM, which fills its channel, the level alone.
    const l = view.level;
    const over = l && l.over_db !== null ? Math.max(0, Math.min(METER_DB, l.over_db)) : null;
    fill.style.width = `${l ? (over !== null ? over / METER_DB : Math.max(0, Math.min(1, (l.level_dbfs + 70) / 70))) * 100 : 0}%`;
    mark.hidden = !narrow || t.squelch_db === null;
    mark.style.left = `${(t.squelch_db ?? 0) / METER_DB * 100}%`;
    setClass(meter, 'open', !!l && l.open);
    setText(meterSays, !l ? (view.state === 'listening' ? 'No reading yet' : 'No reading: it is not listening')
      : l.over_db === null ? `${l.level_dbfs.toFixed(1)} dBFS`
        : `${l.over_db.toFixed(1)} dB over the floor · ${t.squelch_db === null ? 'no squelch' : l.open ? 'squelch open' : 'squelch shut'}`);
    setText(state, stateLine(view));
    setClass(state, 'warn', view.state === 'error');
  }

  const stop = want({ analog: true }, m => {
    view = m.analog;
    draw();
  });
  api.analog().then(v => {
    if (gone || (view && view.rev >= v.rev)) return;
    view = v;
    draw();
  }).catch(e => { if (!gone) toast(e.message, true); });

  return {
    update() {},
    unmount() {
      gone = true;
      stop();
      unplay();
      for (const kind of ['pointerdown', 'keydown']) document.removeEventListener(kind, resume, { capture: true });
    },
  };
}
