// HTTP / WebSocket client for p25-httpd. Every endpoint the UI uses is
// listed here (see doc/P25_API.md). Shapes of /api/ui/* are documented
// in p25-json/src/ui.rs:
//
//   UiState  { v, build, now_unix_ms, clock_valid, uptime_s,
//              site {name, label, nac, wacn, system_id, rfss_id, site_id,
//                    cc_freq_hz, modulation, acquired, last_tsbk_age_ms,
//                    tsbk_per_s, tsbk_ok_pct, health},
//              call {call_id, tg, tg_alias, source, source_alias, sources,
//                    freq_hz, channel, encrypted, started_unix_ms,
//                    elapsed_ms, phase, voice_ms, first_voice_unix_ms,
//                    last_voice_unix_ms, close_in_ms, close_via,
//                    close_window_ms, end_lc, recording} | null,
//              chain {state, parked_freq_hz, follower_enabled, lock_freq,
//                     delivery_mode},
//              recording {enabled, max_count, count, storage, sd_state,
//                         sd_count, ram_count},
//              audio {listeners, lag_total},
//              calls_rev, settings_rev, log_last_seq }
//   UiCalls  { now_unix_ms, calls_rev, recording_enabled, items: [
//              {call_id, tg, tg_alias, source, source_alias, sources,
//               freq_hz, channel, started_unix_ms, ended_unix_ms, open_ms,
//               voice_ms, air_ms, first_voice_ms, imbe, ldu,
//               vocoder_errors, vocoder_silent, encrypted, not_followed,
//               close_reason, recording {id, url, duration_ms, size_bytes,
//               filename, storage} | null, audio_status}] }

const TIMEOUT_MS = 5000;

export class HttpError extends Error {
  constructor(status, body) {
    super((body && (body.error || body.message)) || ('HTTP ' + status));
    this.status = status;
    this.body = body;
  }
}

async function request(method, path, body, timeoutMs = TIMEOUT_MS) {
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), timeoutMs);
  const init = { method, signal: ctl.signal, cache: 'no-store' };
  if (body !== undefined) {
    init.headers = { 'Content-Type': 'application/json' };
    init.body = JSON.stringify(body);
  }
  try {
    const res = await fetch(path, init);
    const text = await res.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch { json = { error: text.slice(0, 200) }; }
    if (!res.ok) throw new HttpError(res.status, json);
    return json;
  } finally {
    clearTimeout(timer);
  }
}

export const get = (path, timeoutMs) => request('GET', path, undefined, timeoutMs);
export const put = (path, body) => request('PUT', path, body);
export const post = (path, body) => request('POST', path, body, 20000);

function qs(params) {
  const p = Object.entries(params).filter(([, v]) => v !== undefined && v !== null && v !== '');
  return p.length ? '?' + p.map(([k, v]) => encodeURIComponent(k) + '=' + encodeURIComponent(v)).join('&') : '';
}

export const api = {
  // Consolidated UI documents.
  state: () => get('/api/ui/state'),
  calls: (limit, nf) => get('/api/ui/calls' + qs({ limit, nf: nf ? 1 : 0 })),
  settings: () => get('/api/ui/settings'),
  putSettings: patch => put('/api/ui/settings', patch),

  // Radio / RF.
  stats: () => get('/api/stats'),
  system: () => get('/api/system'),
  ppm: () => get('/api/ppm'),
  presets: () => get('/api/presets'),
  modulation: () => get('/api/modulation'),
  sites: () => get('/api/sites'),
  setModulation: m => put('/api/modulation' + qs({ set: m })),
  rxGain: (mode, db) => put('/api/rx_gain' + qs({ mode, db })),
  tune: body => post('/api/tune', body),
  applyPreset: body => post('/api/preset', body),
  setSite: name => post('/api/site' + qs({ name })),
  ppmCalibrate: () => post('/api/ppm_calibrate'),
  ppmAuto: (enabled, anchor) => post('/api/ppm/auto' + qs({ enabled: enabled ? 1 : 0, anchor })),
  spectrumWide: () => get('/api/spectrum_wide', 8000),
  spectrum: (chain, fft, averages) => get('/api/spectrum' + qs({ chain, fft, averages }), 8000),

  // Diagnostics.
  pipeline: () => get('/api/pipeline'),
  dibitDelivery: () => get('/api/dibit_delivery'),
  traffic: () => get('/api/traffic'),
  sysHealth: () => get('/api/sys_health'),
  decoderCompare: () => get('/api/decoder_compare'),
  endpoints: () => get('/api/endpoints'),
  log: params => get('/api/log' + qs(params)),
  recordingEvents: id => get('/api/recordings/' + id + '/events'),

  // Talkgroups.
  grantMap: () => get('/api/grant_map'),
  encryptedTgs: () => get('/api/encrypted_tgs'),
  editEncryptedTgs: params => put('/api/encrypted_tgs' + qs(params)),

  // Clock.
  setTime: ms => post('/api/set_time' + qs({ unix_ms: ms })),
};

export function wsUrl(path) {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return proto + '//' + location.host + path;
}

// /ws/events with exponential-backoff reconnect (1 s -> 15 s).
// onEvent(obj) per JSON message; onStatus(bool connected).
// Returns a function that closes the socket for good.
export function openEvents(onEvent, onStatus) {
  let ws = null;
  let backoff = 1000;
  let timer = null;
  let closed = false;
  const connect = () => {
    if (closed) return;
    ws = new WebSocket(wsUrl('/ws/events'));
    ws.onopen = () => { backoff = 1000; onStatus && onStatus(true); };
    ws.onmessage = e => {
      let evt = null;
      try { evt = JSON.parse(e.data); } catch { return; }
      if (evt && evt.event_type !== 'ws_lag') onEvent(evt);
    };
    ws.onclose = () => {
      onStatus && onStatus(false);
      if (closed) return;
      timer = setTimeout(connect, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };
    ws.onerror = () => { try { ws.close(); } catch { /* ignore */ } };
  };
  connect();
  return () => {
    closed = true;
    clearTimeout(timer);
    if (ws) { try { ws.close(); } catch { /* ignore */ } }
  };
}
