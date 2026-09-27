// "Diagnostics": decode-chain counters, dibit delivery (054), the live
// traffic chain, board health, the event log, and raw JSON endpoints.
// Polled only while this view is open.

import { h, card } from '../dom.js';
import { api } from '../api.js';
import { num, pct, bytes, uptime, pllHz, mhz, DASH } from '../format.js';
import { kvTable } from '../components/kv_table.js';
import { logView } from '../components/log_view.js';

const POLL_MS = 2000;

function ratio(a, b) {
  return b ? pct(100 * a / b) : DASH;
}

function chainRows(p) {
  const fc = (p.framers || {}).control || {};
  const ft = (p.framers || {}).traffic || {};
  const q = p.imbe_queue || {};
  const v = p.vocoder || {};
  const r = p.recorder || {};
  const tsbk = (fc.tsbk_crc_ok || 0) + (fc.tsbk_crc_failures || 0);
  return [
    ['--', 'Control channel framer'],
    ['NID ok / attempts', num(fc.nid_decoded_ok) + ' / ' + num(fc.nid_attempts)],
    ['TSBK CRC ok', num(fc.tsbk_crc_ok) + ' (' + ratio(fc.tsbk_crc_ok || 0, tsbk) + ')', tsbk && fc.tsbk_crc_ok / tsbk < 0.9 ? 'warn' : ''],
    ['--', 'Traffic framer'],
    ['NID ok / attempts', num(ft.nid_decoded_ok) + ' / ' + num(ft.nid_attempts)],
    ['NAC mismatch / entropy rej.', num(ft.nid_nac_mismatch) + ' / ' + num(ft.nid_entropy_rejected)],
    ['--', 'Voice path'],
    ['IMBE submitted / consumed', num(q.frames_submitted) + ' / ' + num(q.frames_consumed)],
    ['IMBE dropped (queue full)', num(q.frames_dropped), q.frames_dropped ? 'bad' : ''],
    ['Queue depth / high water', (q.depth ?? DASH) + ' / ' + (q.high_water ?? DASH) + ' of ' + (q.capacity ?? DASH)],
    ['Vocoder errors / silent', num(v.errors_over_4bit) + ' / ' + num(v.silent_observed)],
    ['Encrypted frames skipped', num(v.encrypted_skipped)],
    ['Audio broadcast lag', num((p.audio_broadcast || {}).lag_total), (p.audio_broadcast || {}).lag_total ? 'warn' : ''],
    ['--', 'Recorder'],
    ['Recordings kept', (r.store_count ?? DASH) + ' / ' + (r.store_max ?? DASH)],
    ['Tracker events lagged', num(r.boundary_lag), r.boundary_lag ? 'bad' : ''],
  ];
}

function ringRows(label, d) {
  if (!d) return [];
  const a = d.age || {};
  const k = d.counters || {};
  const clk = d.clock || {};
  const rows = [
    ['--', label + ' ring (' + d.active_mode + (d.requested_mode !== d.active_mode ? ' → ' + d.requested_mode : '') + ')'],
    ['Dibit age p50 / p99 / max', [a.p50_ms, a.p99_ms, a.max_ms].map(x => x != null ? Math.round(x) : DASH).join(' / ') + ' ms', a.p99_ms > 500 ? 'warn' : ''],
    ['Clock uncertainty', clk.uncertainty_ms != null ? clk.uncertainty_ms.toFixed(1) + ' ms' : DASH],
    ['Resyncs / phase mismatches', num(k.resyncs) + ' / ' + num(k.phase_mismatches), (k.resyncs || k.phase_mismatches) ? 'warn' : ''],
    ['Clock reseeds', num(clk.reseeds)],
  ];
  if (label === 'Traffic') {
    rows.push(['Epoch cuts applied / clamped', num(k.cuts_applied) + ' / ' + num(k.cuts_clamped), k.cuts_clamped ? 'warn' : '']);
    rows.push(['Dibits fed / gated / pre-settle', num(k.dibits_fed) + ' / ' + num(k.dibits_gated) + ' / ' + num(k.dibits_discarded_presettle)]);
  }
  return rows;
}

function trafficRows(t) {
  const ch = t.traffic_lsm_chain || {};
  const cc = t.control_lsm_agc || {};
  return [
    ['Follower state', (t.state || DASH) + (t.follower_enabled ? '' : ' (follower OFF)') + (t.lock_freq ? ' (locked)' : '')],
    ['Parked on', mhz(t.parked_freq_hz)],
    ['Last DUID / NAC', (t.last_duid_label || DASH) + ' / ' + (t.last_nac_hex || DASH)],
    ['Traffic PLL', pllHz(ch.pll_dbg)],
    ['Traffic AGC product', ch.agc_product != null ? ch.agc_product.toFixed(3) : DASH, ch.agc_product != null && ch.agc_product < 0.3 ? 'warn' : ''],
    ['Traffic sync distance', ch.sync_distance ?? DASH],
    ['Control PLL / AGC', pllHz(cc.pll_dbg) + ' / ' + (cc.agc_product != null ? cc.agc_product.toFixed(3) : DASH)],
    ['Grants seen / retunes', num(t.grants_seen_new) + ' / ' + num(t.retunes)],
    ['Encrypted grants skipped', num(t.grants_rejected_encrypted)],
  ];
}

function healthRows(s) {
  return [
    ['Uptime', uptime(s.uptime_secs)],
    ['Load (1 / 5 / 15 min)', [s.loadavg_1, s.loadavg_5, s.loadavg_15].map(x => x != null ? x.toFixed(2) : DASH).join(' / ')],
    ['Daemon RSS / threads', (s.daemon_rss_kib != null ? bytes(s.daemon_rss_kib * 1024) : DASH) + ' / ' + (s.daemon_threads ?? DASH)],
    ['Memory available', s.mem_available_pct != null ? pct(s.mem_available_pct, 0) : DASH],
    ['/tmp free (recordings)', bytes(s.disk_tmp_avail_bytes), s.disk_tmp_avail_pct != null && s.disk_tmp_avail_pct < 10 ? 'bad' : ''],
    ['SD card free', bytes(s.disk_sd_avail_bytes)],
  ];
}

function endpointsCard() {
  const c = card('Raw JSON');
  const list = h('div', { class: 'row' });
  c.body.append(list, h('p', { class: 'card-note' }, 'Read-only endpoints (open in a new tab). Full reference: doc/P25_API.md.'));
  api.endpoints().then(r => {
    const items = (r.items || []).filter(e => e.method === 'GET' && !e.params && !e.path.includes('{') && e.path.startsWith('/api/'));
    list.replaceChildren(...items.map(e => h('a', { class: 'chip', href: e.path, target: '_blank', rel: 'noopener', title: e.description, text: e.path.replace('/api/', '') })));
  }).catch(() => list.replaceChildren(h('span', { class: 'dim', text: 'endpoint list unavailable' })));
  return c.el;
}

export function mount(host) {
  const mk = title => { const c = card(title); const kv = kvTable(); c.body.append(kv.el); return { el: c.el, kv }; };
  const chain = mk('Decode chain');
  const delivery = mk('Dibit delivery');
  const traffic = mk('Traffic chain (live)');
  const health = mk('Board');
  const log = logView();
  host.append(
    h('div', { class: 'grid-2' },
      h('div', { class: 'stack' }, chain.el),
      h('div', { class: 'stack' }, traffic.el, delivery.el, health.el)),
    log.el,
    endpointsCard(),
  );
  let timer = null;
  async function poll() {
    if (document.hidden) return;
    const [p, d, t, s] = await Promise.allSettled([api.pipeline(), api.dibitDelivery(), api.traffic(), api.sysHealth()]);
    if (p.status === 'fulfilled') chain.kv.set(chainRows(p.value));
    if (d.status === 'fulfilled') delivery.kv.set([...ringRows('Control', d.value.control), ...ringRows('Traffic', d.value.traffic)]);
    if (t.status === 'fulfilled') traffic.kv.set(trafficRows(t.value));
    if (s.status === 'fulfilled' && !s.value.error) health.kv.set(healthRows(s.value));
  }
  poll();
  timer = setInterval(poll, POLL_MS);
  return {
    update() {},
    unmount() { clearInterval(timer); log.unmount(); },
  };
}
