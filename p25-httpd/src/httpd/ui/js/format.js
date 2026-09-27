// Formatting helpers. Times from the radio are the BOARD clock (often
// 1970 until something sets it), so ages are computed against the
// board's own `now_unix_ms`, never against the browser clock.

export const DASH = '—';

export function mhz(hz, digits = 4) {
  if (hz === null || hz === undefined || hz === 0) return DASH;
  return (hz / 1e6).toFixed(digits) + ' MHz';
}

export function khz(hz, digits = 1) {
  if (hz === null || hz === undefined) return DASH;
  return (hz / 1e3).toFixed(digits) + ' kHz';
}

// 1.4 s, 12 s, 2m 05s, 1h 03m
export function dur(ms) {
  if (ms === null || ms === undefined) return DASH;
  if (ms < 10000) return (ms / 1000).toFixed(1) + ' s';
  const s = Math.round(ms / 1000);
  if (s < 60) return s + ' s';
  const m = Math.floor(s / 60);
  if (m < 60) return m + 'm ' + String(s % 60).padStart(2, '0') + 's';
  const hh = Math.floor(m / 60);
  return hh + 'h ' + String(m % 60).padStart(2, '0') + 'm';
}

// "just now", "12 s ago", "4 min ago", "2 h ago"
export function ago(ms) {
  if (ms === null || ms === undefined || ms < 0) return DASH;
  if (ms < 2000) return 'just now';
  const s = Math.round(ms / 1000);
  if (s < 90) return s + ' s ago';
  const m = Math.round(s / 60);
  if (m < 90) return m + ' min ago';
  return Math.round(m / 60) + ' h ago';
}

export function uptime(s) {
  if (s === null || s === undefined) return DASH;
  const d = Math.floor(s / 86400);
  const hh = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return d + 'd ' + hh + 'h';
  if (hh > 0) return hh + 'h ' + m + 'm';
  return m + 'm ' + (s % 60) + 's';
}

// Wall clock HH:MM:SS of a board timestamp, only when the board clock
// is valid; otherwise null (callers show a relative age instead).
export function clock(unixMs, clockValid) {
  if (!clockValid || !unixMs) return null;
  const d = new Date(unixMs);
  const p = n => String(n).padStart(2, '0');
  return p(d.getHours()) + ':' + p(d.getMinutes()) + ':' + p(d.getSeconds());
}

export function clockMs(unixMs) {
  const d = new Date(unixMs);
  const p = (n, w = 2) => String(n).padStart(w, '0');
  return p(d.getHours()) + ':' + p(d.getMinutes()) + ':' + p(d.getSeconds()) + '.' + p(d.getMilliseconds(), 3);
}

export function bytes(n) {
  if (n === null || n === undefined) return DASH;
  if (n < 1024) return n + ' B';
  if (n < 1048576) return (n / 1024).toFixed(0) + ' KB';
  if (n < 1073741824) return (n / 1048576).toFixed(1) + ' MB';
  return (n / 1073741824).toFixed(2) + ' GB';
}

export function num(v, digits = 0) {
  if (v === null || v === undefined || Number.isNaN(v)) return DASH;
  return Number(v).toLocaleString(undefined, { maximumFractionDigits: digits, minimumFractionDigits: digits });
}

export function pct(v, digits = 1) {
  if (v === null || v === undefined) return DASH;
  return Number(v).toFixed(digits) + ' %';
}

// "TG 300" / "EMS Dispatch (300)"
export function tgLabel(tg, alias) {
  if (!tg) return DASH;
  return alias ? alias + ' (' + tg + ')' : 'TG ' + tg;
}

export function unitLabel(id, alias) {
  if (!id) return 'unknown source';
  return alias ? alias + ' (' + id + ')' : String(id);
}

// `sources` lists every unit seen on the call in order, the grant's unit
// first. `source` is the talker the voice's link control names. On this
// site a dispatch console (1011-1014) often talks on a grant issued to
// another unit: that is one talker, so the grant's unit is labelled as
// such, and only further talkers read "also".
export function otherUnits(call) {
  const src = call.sources || [];
  const grant = src.length && src[0] !== call.source ? src[0] : null;
  const also = src.slice(1).filter(s => s !== call.source && s !== grant);
  return { grant, also };
}

export function otherUnitsText(call) {
  const { grant, also } = otherUnits(call);
  return [grant ? 'grant ' + grant : '', also.length ? 'also ' + also.join(', ') : '']
    .filter(Boolean).join(' · ');
}

// Q2.13 PLL register -> Hz at 4800 sym/s.
export function pllHz(q213) {
  if (q213 === null || q213 === undefined) return DASH;
  return Math.round(q213 * 4800 / (2 * Math.PI * 8192)) + ' Hz';
}

export const NOT_FOLLOWED = {
  encrypted: 'encrypted',
  sticky_lock: 'busy on another call',
  monitor_list: 'not on monitor list',
  traffic_lock: 'chain locked (diagnostic)',
  update_no_lock: 'update without grant',
};

// Change 057: why the lifecycle closed a call (`close_reason`).
export const CLOSE_REASON = {
  call_end: 'end of transmission',
  tg_change: 'next grant',
  timeout: 'no activity',
  stream_lag: 'event lag',
  sync_lost: 'sync lost',
};

export const AUDIO_STATUS = {
  recorded: '',
  saving: 'saving…',
  not_recorded: 'recording off',
  evicted: 'recording rotated out',
  no_voice: 'no voice decoded',
  encrypted: 'encrypted',
  not_followed: 'not followed',
  missing: 'no recording',
};
