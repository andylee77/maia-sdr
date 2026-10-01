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
  profiles: () => request('GET', '/api/v1/profiles'),
  selectProfile: (site, profile) => request('PUT', `/api/v1/sites/${encodeURIComponent(site)}/profile`, { profile }),
  routes: () => request('GET', '/api/v1/routes'),
  calls: () => request('GET', '/api/v1/calls'),
  recordings: limit => request('GET', `/api/v1/recordings?limit=${limit}`),
  setRecording: recording => request('PUT', '/api/v1/radio/recording', recording),
  clearRecordings: store => request('DELETE', `/api/v1/recordings?store=${store}`),
  events: (after, routine) => request('GET', `/api/v1/events?after=${after}&routine=${!!routine}`),
};
