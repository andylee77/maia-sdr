// "Radio": RF front end and control-channel tuning — site, frequency,
// preset, gain, PPM correction, modulation, spectrum. Everything here
// changes the receiver, so every action asks first.

import { h, setText, card, toast, switchInput } from '../dom.js';
import { api } from '../api.js';
import { kick } from '../store.js';
import { mhz, khz, ago, DASH } from '../format.js';
import { kvTable } from '../components/kv_table.js';
import { spectrumCard } from '../components/spectrum.js';
import { coverageCard } from '../components/coverage_card.js';

const POLL_MS = 2000;
const GAINS = [0, 10, 20, 30, 40, 50, 60, 70, 76];

function option(value, text) {
  const o = h('option', { value, text });
  return o;
}

function tuningCard() {
  const c = card('Control channel');
  const kv = kvTable();
  const freq = h('input', { class: 'input wide', type: 'number', step: '0.00625', 'aria-label': 'Control channel MHz' });
  const step = h('select', { class: 'input', 'aria-label': 'Step' }, option('12500', '12.5 kHz'), option('6250', '6.25 kHz'));
  const down = h('button', { class: 'btn icon', type: 'button', title: 'Step down', text: '−' });
  const up = h('button', { class: 'btn icon', type: 'button', title: 'Step up', text: '+' });
  const tune = h('button', { class: 'btn primary', type: 'button', text: 'Tune' });
  const center = h('select', { class: 'input', 'aria-label': 'LO centre mode' },
    option('auto', 'LO: auto-recentre'), option('lock', 'LO: locked'));
  const preset = h('select', { class: 'input', 'aria-label': 'DDC preset' });
  const applyPreset = h('button', { class: 'btn', type: 'button', text: 'Apply preset' });
  const site = h('select', { class: 'input', 'aria-label': 'Site' });
  const applySite = h('button', { class: 'btn', type: 'button', text: 'Switch site' });
  c.body.append(kv.el,
    h('div', { class: 'row', style: { marginTop: '12px' } }, freq, down, up, step, tune),
    h('div', { class: 'row', style: { marginTop: '8px' } }, center),
    h('div', { class: 'row', style: { marginTop: '8px' } }, preset, applyPreset),
    h('div', { class: 'row', style: { marginTop: '8px' } }, site, applySite));

  async function doTune(hz) {
    if (!confirm('Retune the control channel to ' + mhz(hz, 5) + '?\nDecoding stops if this is not the site’s control channel.')) return;
    try {
      const r = await api.tune({ radio_freq_hz: hz, center_mode: center.value });
      toast('Tuned ' + mhz(hz, 5) + (r.lo_moved ? ' (LO moved)' : ''));
      kick(300);
    } catch (e) {
      toast(e.status === 409 ? 'Outside the locked LO window' : 'Tune failed: ' + e.message, true);
    }
  }
  const typed = () => Math.round(parseFloat(freq.value) * 1e6);
  tune.addEventListener('click', () => { const hz = typed(); if (hz > 7e7) doTune(hz); });
  freq.addEventListener('keydown', e => { if (e.key === 'Enter') tune.click(); });
  down.addEventListener('click', () => { const hz = typed(); if (hz > 7e7) doTune(hz - Number(step.value)); });
  up.addEventListener('click', () => { const hz = typed(); if (hz > 7e7) doTune(hz + Number(step.value)); });
  applyPreset.addEventListener('click', async () => {
    if (!confirm('Apply preset ' + preset.value + '? The AD9361 and DDC are reconfigured (a few seconds without decode).')) return;
    try { const r = await api.applyPreset({ preset: preset.value }); toast('Preset ' + r.preset + ' applied'); kick(500); } catch (e) { toast('Preset failed: ' + e.message, true); }
  });
  applySite.addEventListener('click', async () => {
    if (!confirm('Switch to site ' + site.options[site.selectedIndex].text + '?')) return;
    try {
      const r = await api.setSite(site.value);
      if (!r.ok) throw new Error(r.error || 'rejected');
      // Change 070: the planner picks the preset and LO for the site's
      // channels (the site file's preset if it cannot).
      try {
        await api.applyPreset({ preset: 'auto' });
      } catch {
        if (r.site && r.site.preset_default) await api.applyPreset({ preset: r.site.preset_default });
      }
      toast('Site switched'); kick(500);
    } catch (e) { toast('Site switch failed: ' + e.message, true); }
  });

  async function loadOnce() {
    try {
      const p = await api.presets();
      preset.replaceChildren(...p.presets.map(x => option(x.name, x.name + ' · ' + (x.sample_rate_hz / 1e6) + ' MSPS / ' + (x.rf_bandwidth_hz / 1e6) + ' MHz BW')));
      preset.value = p.current || p.default;
      center.value = p.center_locked ? 'lock' : 'auto';
    } catch { /* retried on next mount */ }
    try {
      const s = await api.sites();
      site.replaceChildren(...s.sites.map(x => option(x.name, x.label)));
      if (s.active) site.value = s.active;
    } catch { /* ignore */ }
  }

  function update(st) {
    if (document.activeElement !== freq && st.radio_freq_hz) freq.value = (st.radio_freq_hz / 1e6).toFixed(6);
    kv.set([
      ['Tuned', mhz(st.radio_freq_hz, 5)],
      ['RX LO', mhz(st.rx_lo_hz, 4)],
      ['NCO offset', khz(st.ddc_control_offset_hz, 2)],
      ['RF bandwidth', mhz(st.rf_bandwidth_hz, 1)],
      ['Sample rate', st.sampling_frequency_hz ? (st.sampling_frequency_hz / 1e6).toFixed(2) + ' MSPS' : DASH],
      ['DDC', (st.ddc_decimation || DASH) + (st.ddc_output_rate_hz ? ' → ' + (st.ddc_output_rate_hz / 1e3).toFixed(1) + ' kSPS' : '')],
    ]);
  }
  loadOnce();
  return { el: c.el, update };
}

function gainCard() {
  const c = card('Gain');
  const kv = kvTable();
  let busy = false;
  const agc = switchInput('AGC (slow attack)', false, async on => {
    busy = true;
    try { await api.rxGain(on ? 'slow_attack' : 'manual'); } catch (e) { toast('AGC change failed: ' + e.message, true); }
    busy = false;
  });
  const sel = h('select', { class: 'input', 'aria-label': 'Manual gain' }, GAINS.map(g => option(String(g), g + ' dB')));
  sel.addEventListener('change', async () => {
    busy = true;
    try { await api.rxGain('manual', Number(sel.value)); } catch (e) { toast('Gain change failed: ' + e.message, true); }
    busy = false;
  });
  c.body.append(kv.el, h('div', { class: 'row', style: { marginTop: '12px' } }, agc.el, sel));

  function update(st) {
    const manual = st.gain_control_mode === 'manual';
    if (!busy && document.activeElement !== agc.input) agc.input.checked = !manual;
    sel.disabled = !manual;
    if (!busy && document.activeElement !== sel && st.rx_gain_db != null) {
      let best = GAINS[0];
      for (const g of GAINS) if (Math.abs(g - st.rx_gain_db) < Math.abs(best - st.rx_gain_db)) best = g;
      sel.value = String(best);
    }
    kv.set([
      ['Gain', st.rx_gain_db != null ? st.rx_gain_db.toFixed(0) + ' dB (' + (st.gain_control_mode || DASH) + ')' : DASH],
      ['RSSI (AD9361)', st.rx_rssi_db != null ? st.rx_rssi_db.toFixed(1) + ' dB' : DASH],
    ]);
  }
  return { el: c.el, update };
}

function ppmCard() {
  const c = card('Frequency correction');
  const kv = kvTable();
  let busy = false;
  const anchor = h('select', { class: 'input', 'aria-label': 'Anchor window' },
    ['25', '50', '100', '200', '500'].map(v => option(v, '±' + v + ' Hz')));
  const auto = switchInput('Auto-track', true, on => setAuto(on));
  anchor.addEventListener('change', () => setAuto(auto.input.checked));
  const cal = h('button', { class: 'btn', type: 'button', text: 'Recalibrate' });
  c.body.append(kv.el, h('div', { class: 'row', style: { marginTop: '12px' } }, auto.el, anchor, h('div', { class: 'spacer' }), cal));

  async function setAuto(on) {
    busy = true;
    try { await api.ppmAuto(on, anchor.value); } catch (e) { toast('Auto-PPM change failed: ' + e.message, true); }
    busy = false;
  }
  cal.addEventListener('click', async () => {
    if (!confirm('Run a PPM calibration now (about 7 s)?')) return;
    cal.disabled = true; setText(cal, 'Calibrating…');
    try {
      const r = await api.ppmCalibrate();
      if (!r.ok || !r.result) throw new Error(r.error || 'no result');
      toast('Calibrated: shift ' + Math.round(r.result.final_lo_shift_hz) + ' Hz (' + r.result.final_lo_ppm.toFixed(3) + ' ppm)');
    } catch (e) { toast('Calibration failed: ' + e.message, true); }
    cal.disabled = false; setText(cal, 'Recalibrate');
  });

  function update(p, clockValid) {
    if (!busy) {
      if (document.activeElement !== auto.input) auto.input.checked = !!p.auto_ppm_enabled;
      if (document.activeElement !== anchor && p.auto_ppm_anchor_hz != null) anchor.value = String(p.auto_ppm_anchor_hz);
    }
    const wa = p.would_apply || {};
    const age = p.calibrated_this_session && clockValid && p.last_cal_unix_secs > 1e9
      ? ago(Date.now() - p.last_cal_unix_secs * 1000) : (p.calibrated_this_session ? 'this session' : 'stored / boot value');
    kv.set([
      ['Applied', (p.lo_shift_hz >= 0 ? '+' : '') + Math.round(p.lo_shift_hz) + ' Hz (' + p.lo_ppm.toFixed(3) + ' ppm)', 'good'],
      ['Calibrated', age],
      ['Tracker', p.tracker_estimate_hz != null ? Math.round(p.tracker_estimate_hz) + ' Hz (' + (p.tracker_samples || 0) + ' samples)' : 'collecting (' + (p.tracker_samples || 0) + ')'],
      ['Auto apply', (wa.reason || DASH).replace(/_/g, ' '), wa.reason === 'outside_anchor' ? 'warn' : ''],
    ]);
  }
  return { el: c.el, update };
}

function modulationCard() {
  const c = card('Modulation');
  const sel = h('select', { class: 'input', 'aria-label': 'Modulation' },
    option('auto', 'Auto-detect'), option('lsm', 'LSM (simulcast)'), option('c4fm', 'C4FM'));
  const info = h('div', { class: 'dim', style: { fontSize: '0.9em', marginTop: '8px' } });
  sel.addEventListener('change', async () => {
    try { await api.setModulation(sel.value); toast('Modulation: ' + sel.value); } catch (e) { toast('Failed: ' + e.message, true); }
  });
  c.body.append(sel, info);
  // Change 071b: both decoders run; the counts show why auto picked one.
  let prev = null;
  function update(m) {
    const v = ({ 0: 'auto', 1: 'c4fm', 2: 'lsm' })[m.mode] || 'auto';
    if (document.activeElement !== sel) sel.value = v;
    const ok = m.tsbk_ok || {};
    const fail = m.tsbk_fail || {};
    const rate = k => {
      if (!prev) return DASH;
      const o = (ok[k] || 0) - (prev.ok[k] || 0);
      const f = (fail[k] || 0) - (prev.fail[k] || 0);
      return o + f > 0 ? Math.round((100 * o) / (o + f)) + '%' : DASH;
    };
    const sw = m.c4fm_software || {};
    setText(info, 'Decoding ' + m.label + (m.mode === 0 ? ' (auto)' : '') +
      ' · control messages passing CRC: LSM ' + rate('lsm') + ', C4FM ' + rate('c4fm') +
      (sw.cpu_pct != null ? ' · C4FM demodulator ' + sw.cpu_pct.toFixed(1) + '% CPU' : ''));
    prev = { ok, fail };
  }
  return { el: c.el, update };
}

export function mount(host) {
  const tuning = tuningCard();
  const gain = gainCard();
  const ppm = ppmCard();
  const mod = modulationCard();
  const spec = spectrumCard();
  const coverage = coverageCard();
  host.append(
    h('div', { class: 'grid-2' }, h('div', { class: 'stack' }, tuning.el, coverage.el), h('div', { class: 'stack' }, gain.el, ppm.el, mod.el)),
    spec.el,
  );
  let clockValid = false;
  let timer = null;
  async function poll() {
    if (document.hidden) return;
    const [st, p, m] = await Promise.allSettled([api.stats(), api.ppm(), api.modulation()]);
    if (st.status === 'fulfilled') { tuning.update(st.value); gain.update(st.value); }
    if (p.status === 'fulfilled' && p.value.ok) ppm.update(p.value, clockValid);
    if (m.status === 'fulfilled') mod.update(m.value);
  }
  poll();
  timer = setInterval(poll, POLL_MS);
  // Change 070: grant counts change slowly.
  const covTimer = setInterval(() => { if (!document.hidden) coverage.load(); }, 10000);
  return {
    update(kind, store) { if (kind === 'state' && store.state) clockValid = store.state.clock_valid; },
    unmount() { clearInterval(timer); clearInterval(covTimer); spec.unmount(); },
  };
}
