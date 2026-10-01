// Site / control-channel health card (Now view).

import { h, setText, setClass, card } from '../dom.js';
import { mhz, ago, dur, siteClock, utcOffset, CLOCK_SOURCE } from '../format.js';
import { store } from '../store.js';
import { metric } from './kv_table.js';

const HEALTH_TEXT = {
  ok: 'Control channel decoding',
  stale: 'Control channel silent',
  searching: 'Searching for the control channel',
};

// Metric labels per protocol (change 075: DMR sites).
const LABELS = {
  p25: { rate: 'TSBK / s', crc: 'CRC ok', last: 'Last TSBK', mod: 'Modulation' },
  dmr: { rate: 'Messages / s', crc: 'Valid', last: 'Last message', mod: 'DMR Tier III' },
};

export function siteCard() {
  const c = card('Site');
  const dot = h('span', { class: 'dot' });
  const name = h('span', { class: 'site-name', text: '—' });
  const ident = h('span', { class: 'dim' });
  const health = h('div', { class: 'dim', style: { marginTop: '4px' } });
  const m = {
    cc: metric('Control ch'),
    rate: metric('TSBK / s'),
    crc: metric('CRC ok'),
    last: metric('Last TSBK'),
    mod: metric('Modulation'),
    traffic: metric('Traffic chain'),
    // Change 067: the site's time (control channel) and the radio clock.
    time: metric('Site time'),
    clock: metric('Radio clock'),
  };
  c.body.append(
    h('div', { class: 'site-main' }, dot, name, ident),
    health,
    h('div', { class: 'site-metrics' }, Object.values(m).map(x => x.el)),
  );

  // Clock offsets span seconds (drift) to months (a replayed site).
  const span = ms => ms >= 172800000 ? Math.round(ms / 86400000) + ' days' : dur(ms);
  let last = null;
  // The site time ticks between state polls.
  function tick() {
    const t = last && last.site.site_time;
    if (!t) { m.time.set('—'); return; }
    const ms = t.unix_ms + (Date.now() - store.receivedAt);
    m.time.set(siteClock(ms));
    m.time.el.title = 'From the control channel (' + (t.precision === 'minute' ? 'to the minute so far' : t.precision === 'second' ? 'to the second' : 'precise')
      + (t.ext_locked ? ', site locked to GPS' : ', site clock not GPS-locked')
      + (t.local_offset_min != null ? ', site announces ' + utcOffset(t.local_offset_min) : '') + "); shown in this browser's time zone";
  }
  const timer = setInterval(tick, 1000);

  let proto = 'p25';
  function update(s) {
    const site = s.site;
    last = s;
    tick();
    const p = site.protocol === 'dmr' ? 'dmr' : 'p25';
    if (p !== proto) {
      proto = p;
      for (const [k, t] of Object.entries(LABELS[p])) m[k].label(t);
    }
    const src = site.clock_source || 'manual';
    const t = site.site_time;
    const off = t ? t.board_offset_ms : 0;
    m.clock.set((CLOCK_SOURCE[src] || src)
      + (t && Math.abs(off) > 2000 ? ' · ' + span(Math.abs(off)) + (off > 0 ? ' behind' : ' ahead') + ' of the site' : ''));
    setText(name, site.label || site.name || 'Unknown site');
    const bits = [];
    if (site.nac) bits.push('NAC ' + site.nac);
    if (site.wacn) bits.push('WACN ' + site.wacn);
    if (site.system_id) bits.push('SYS ' + site.system_id);
    if (site.rfss_id != null) bits.push('RFSS ' + site.rfss_id + ' / site ' + site.site_id);
    const d = site.dmr;
    if (d) {
      if (d.color_code != null) bits.push('CC ' + d.color_code);
      if (d.network != null) bits.push((d.model || '').toLowerCase() + ' network ' + d.network + ' / site ' + d.site);
    }
    setText(ident, bits.join(' · '));
    setClass(dot, 'ok', site.health === 'ok');
    setClass(dot, 'warn', site.health === 'stale');
    setClass(dot, 'bad', site.health === 'searching');
    setText(health, HEALTH_TEXT[site.health] || site.health);
    m.cc.set(mhz(site.cc_freq_hz));
    if (d) {
      m.rate.set(d.msgs_per_s != null ? d.msgs_per_s.toFixed(1) : '—');
      m.crc.set(d.msgs_ok_pct != null ? d.msgs_ok_pct.toFixed(1) + ' %' : '—');
      m.crc.el.title = d.cach_ok_pct != null ? 'CACH (timeslot channel) ' + d.cach_ok_pct.toFixed(1) + ' % since start' : '';
      m.last.set(d.last_msg_age_ms != null ? ago(d.last_msg_age_ms) : 'never');
      m.mod.set('4FSK' + (d.carrier_offset_hz != null ? ' · ' + (d.carrier_offset_hz > 0 ? '+' : '') + d.carrier_offset_hz + ' Hz' : '') + ' · ' + d.cpu_pct.toFixed(0) + '% CPU');
      const f = d.following;
      m.traffic.set(!d.follow ? 'follower off'
        : f ? (f.private ? 'radio ' : 'TG ') + f.talkgroup + ' · LCN ' + f.lcn + ' TS' + f.timeslot + (f.freq_hz ? ' · ' + mhz(f.freq_hz) : '')
        : 'idle · ' + d.grants_seen + ' grants seen');
      return;
    }
    m.rate.set(site.tsbk_per_s != null ? site.tsbk_per_s.toFixed(1) : '—');
    m.crc.set(site.tsbk_ok_pct != null ? site.tsbk_ok_pct.toFixed(1) + ' %' : '—');
    m.last.set(site.last_tsbk_age_ms != null ? ago(site.last_tsbk_age_ms) : 'never');
    m.mod.set(site.modulation);
    const ch = s.chain;
    const parked = ch.parked_freq_hz ? mhz(ch.parked_freq_hz) : 'not tuned';
    m.traffic.set(ch.follower_enabled ? (ch.lock_freq ? 'locked ' : '') + parked : 'follower off');
  }

  return { el: c.el, update, unmount: () => clearInterval(timer) };
}
