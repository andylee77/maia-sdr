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

  function update(s) {
    const site = s.site;
    last = s;
    tick();
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
    setText(ident, bits.join(' · '));
    setClass(dot, 'ok', site.health === 'ok');
    setClass(dot, 'warn', site.health === 'stale');
    setClass(dot, 'bad', site.health === 'searching');
    setText(health, HEALTH_TEXT[site.health] || site.health);
    m.cc.set(mhz(site.cc_freq_hz));
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
