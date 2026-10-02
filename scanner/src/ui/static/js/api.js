// The /api/v1 calls the pages use. Errors carry the server's message.

// A WebSocket URL on this server.
export function wsUrl(path) {
  return (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + path;
}

async function request(method, path, body) {
  const opts = { method, headers: {} };
  if (body !== undefined) {
    opts.headers['Content-Type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  const res = await fetch(path, opts);
  const data = await res.json().catch(() => null);
  if (!res.ok) throw new Error((data && data.error) || `${method} ${path}: HTTP ${res.status}`);
  return data;
}

export const api = {
  status: () => request('GET', '/api/v1/status'),
  radio: () => request('GET', '/api/v1/radio'),
  setGain: (mode, manual_db) => request('PUT', '/api/v1/radio/gain', { mode, manual_db }),
  systems: () => request('GET', '/api/v1/systems'),
  sites: () => request('GET', '/api/v1/sites'),
  activate: id => request('POST', `/api/v1/sites/${encodeURIComponent(id)}/activate`),
  stopSite: id => request('POST', `/api/v1/sites/${encodeURIComponent(id)}/stop`),
  siteLearned: id => request('GET', `/api/v1/sites/${encodeURIComponent(id)}/learned`),
  data: q => request('GET', '/api/v1/data' + (q ? '?' + q : '')),
  sitePlan: id => request('GET', `/api/v1/sites/${encodeURIComponent(id)}/plan`),
  recentre: id => request('POST', `/api/v1/sites/${encodeURIComponent(id)}/recentre`),
  saveSystem: (system, edit) => request('PUT', `/api/v1/systems/${encodeURIComponent(system)}`, edit),
  saveSite: (system, site, edit) => request('PUT', `/api/v1/systems/${encodeURIComponent(system)}/sites/${encodeURIComponent(site)}`, edit),
  deleteSite: (system, site) => request('DELETE', `/api/v1/systems/${encodeURIComponent(system)}/sites/${encodeURIComponent(site)}`),
  deleteSystem: system => request('DELETE', `/api/v1/systems/${encodeURIComponent(system)}`),
  importConfig: doc => request('PUT', '/api/v1/config', doc),
  factoryReset: () => request('POST', '/api/v1/config/factory-reset'),
  saveRadioSettings: settings => request('PUT', '/api/v1/radio/settings', settings),
  setClockSource: source => request('PUT', '/api/v1/radio/clock', { source }),
  setTime: unix_ms => request('POST', '/api/v1/clock', { unix_ms }),
  crystal: () => request('GET', '/api/v1/radio/crystal'),
  setCrystal: tracking => request('PUT', '/api/v1/radio/crystal', tracking),
  calibrateCrystal: () => request('POST', '/api/v1/radio/crystal/calibrate'),
  routes: () => request('GET', '/api/v1/routes'),
  calls: () => request('GET', '/api/v1/calls'),
  setHold: tg => request('PUT', '/api/v1/hold', { tg }),
  scan: req => request('POST', '/api/v1/scan', req),
  scanOptions: () => request('GET', '/api/v1/scan/options'),
  scanCancel: () => request('POST', '/api/v1/scan/cancel'),
  scanAdd: card => request('POST', '/api/v1/scan/add', card),
  recordings: (limit, site) => request('GET', `/api/v1/recordings?limit=${limit}${site ? `&site=${encodeURIComponent(site)}` : ''}`),
  setRecording: recording => request('PUT', '/api/v1/radio/recording', recording),
  clearRecordings: store => request('DELETE', `/api/v1/recordings?store=${store}`),
  activity: (path, q) => request('GET', `/api/v1/activity/${path}${q ? `?${q}` : ''}`),
};
