// The /api/v1 calls the pages use. Errors carry the server's message and the HTTP `status`.
// A call the radio refuses because this browser is not signed in (any more) tells the shell
// (`onSignedOut`), which shows the sign-in page.

// A WebSocket URL on this server.
export function wsUrl(path) {
  return (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + path;
}

const SESSION = '/api/v1/session';
let signedOut = null;

// `fn()` is called when a call finds this browser signed out.
export function onSignedOut(fn) {
  signedOut = fn;
}

async function request(method, path, body) {
  const opts = { method, headers: {} };
  if (body !== undefined) {
    opts.headers['Content-Type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  const res = await fetch(path, opts);
  const text = await res.text();
  let data = null;
  try {
    data = JSON.parse(text);
  } catch { /* a refusal in plain text, or no body */ }
  if (res.status === 401 && path !== SESSION && signedOut) signedOut();
  if (!res.ok) {
    const e = new Error((data && data.error) || text.trim() || `${method} ${path}: HTTP ${res.status}`);
    e.status = res.status;
    throw e;
  }
  return data;
}

const system = id => `/api/v1/systems/${encodeURIComponent(id)}`;

export const api = {
  // Who this browser is to the radio: { protected, level, via }.
  session: () => request('GET', SESSION),
  signIn: password => request('POST', SESSION, { password }),
  signOut: () => request('DELETE', SESSION),
  // The radio's HTTPS: its port, its CA's name and SHA-256, the names its certificate is for.
  certificate: () => request('GET', '/api/v1/certificate'),
  // The administrator's: the passwords set, the browsers signed in, the tokens, the names.
  access: () => request('GET', '/api/v1/access'),
  // `edit`: { level, new, current }; `new` null takes the password away.
  setPassword: edit => request('PUT', '/api/v1/access/password', edit),
  createToken: (label, level) => request('POST', '/api/v1/access/tokens', { label, level }),
  revokeToken: id => request('DELETE', `/api/v1/access/tokens/${encodeURIComponent(id)}`),
  endSession: id => request('DELETE', `/api/v1/access/sessions/${encodeURIComponent(id)}`),
  endOtherSessions: () => request('DELETE', '/api/v1/access/sessions'),
  setNames: names => request('PUT', '/api/v1/access/names', { names }),
  // libiio's daemon (the board as a tuner for other programs): { available, wanted, running }.
  iiod: () => request('GET', '/api/v1/access/iiod'),
  // Switched for every start.
  setIiod: on => request('PUT', '/api/v1/access/iiod', { on }),
  status: () => request('GET', '/api/v1/status'),
  radio: () => request('GET', '/api/v1/radio'),
  setGain: (mode, manual_db) => request('PUT', '/api/v1/radio/gain', { mode, manual_db }),
  systems: () => request('GET', '/api/v1/systems'),
  sites: () => request('GET', '/api/v1/sites'),
  activate: id => request('POST', `/api/v1/sites/${encodeURIComponent(id)}/activate`),
  stopSite: id => request('POST', `/api/v1/sites/${encodeURIComponent(id)}/stop`),
  siteLearned: id => request('GET', `/api/v1/sites/${encodeURIComponent(id)}/learned`),
  packetData: q => request('GET', '/api/v1/packet_data' + (q ? '?' + q : '')),
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
  setPacketData: follow => request('PUT', '/api/v1/radio/packet_data', { follow }),
  // A system's aliases with their revision: { aliases, listening, rev }.
  aliases: id => request('GET', `${system(id)}/aliases`),
  // The whole list in place of the one read at `rev`; 409 (`status`) when it changed since.
  setAliases: (id, aliases, rev) => request('PUT', `${system(id)}/aliases?rev=${rev}`, aliases),
  // How a system treats talkgroups with no priority, and pre-emption.
  setListening: (id, listening) => request('PUT', `${system(id)}/listening`, listening),
  // A RadioReference file (`req`: csv, encrypted_do_not_monitor, sites) imported, or only what
  // it would change (`preview`).
  radioReference: (id, req, preview) => request('POST', `${system(id)}/radioreference${preview ? '/preview' : ''}`, req),
  // An SDRTrunk playlist (`req`: xml, and what to take of each of its systems) imported, or
  // only what it would change (`preview`).
  playlist: (req, preview) => request('POST', `/api/v1/playlist${preview ? '/preview' : ''}`, req),
  // One talkgroup's controls (`edit`: any of name, priority, follow, record, speaker).
  setTalkgroup: (system, tg, edit) => request('PUT', `/api/v1/systems/${encodeURIComponent(system)}/talkgroups/${tg}`, edit),
  // Its alias removed: it is a talkgroup with no settings of its own again.
  forgetTalkgroup: (system, tg) => request('DELETE', `/api/v1/systems/${encodeURIComponent(system)}/talkgroups/${tg}`),
  setTime: unix_ms => request('POST', '/api/v1/clock', { unix_ms }),
  crystal: () => request('GET', '/api/v1/radio/crystal'),
  setCrystal: tracking => request('PUT', '/api/v1/radio/crystal', tracking),
  calibrateCrystal: () => request('POST', '/api/v1/radio/crystal/calibrate'),
  routes: () => request('GET', '/api/v1/routes'),
  calls: () => request('GET', '/api/v1/calls'),
  // `lane` (1 or 2): hold only that traffic lane.
  setHold: (tg, lane) => request('PUT', '/api/v1/hold', lane ? { tg, lane } : { tg }),
  scan: req => request('POST', '/api/v1/scan', req),
  scanOptions: () => request('GET', '/api/v1/scan/options'),
  scanCancel: () => request('POST', '/api/v1/scan/cancel'),
  scanAdd: card => request('POST', '/api/v1/scan/add', card),
  setMode: mode => request('PUT', '/api/v1/mode', { mode }),
  atscScan: req => request('POST', '/api/v1/atsc/scan', req),
  atscOptions: () => request('GET', '/api/v1/atsc/scan/options'),
  atscCancel: () => request('POST', '/api/v1/atsc/scan/cancel'),
  atscChannel: n => request('GET', `/api/v1/atsc/scan/channel/${n}`),
  atscRecordings: () => request('GET', '/api/v1/atsc/recordings'),
  atscLive: () => request('GET', '/api/v1/atsc/live'),
  atscLiveStart: rf => request('POST', '/api/v1/atsc/live', { rf }),
  atscLiveStop: () => request('POST', '/api/v1/atsc/live/stop'),
  recordings: (limit, site) => request('GET', `/api/v1/recordings?limit=${limit}${site ? `&site=${encodeURIComponent(site)}` : ''}`),
  // Any of the recording settings (`enabled`, `every_call`, the store and its limits).
  setRecording: recording => request('PUT', '/api/v1/radio/recording', recording),
  lockRecording: (id, locked) => request('PUT', `/api/v1/recordings/${id}/lock`, { locked }),
  clearRecordings: store => request('DELETE', `/api/v1/recordings?store=${store}`),
  activity: (path, q) => request('GET', `/api/v1/activity/${path}${q ? `?${q}` : ''}`),
  // Data mode's listening: its state, window, emitters, newest bursts and what is armed.
  data: () => request('GET', '/api/v1/data'),
  // Keep the IQ of the next `count` bursts: any, or only those near `freq_hz`.
  dataArm: (count, freq_hz) => request('PUT', '/api/v1/data/arm', freq_hz ? { count, freq_hz } : { count }),
  dataDisarm: () => request('DELETE', '/api/v1/data/arm'),
  // The bursts' IQ kept on the card: { count, bytes, max_count, max_bytes, items }.
  dataCaptures: () => request('GET', '/api/v1/data/captures'),
  dataDeleteCapture: id => request('DELETE', `/api/v1/data/captures/${encodeURIComponent(id)}`),
  // What is on the card as files to take away; its recordings by spans of `span` minutes.
  files: span => request('GET', `/api/v1/files?span=${span}`),
  // How many recordings a selection (`q`: from, to, site, tg, unit, locked) takes, and their size.
  filesCount: q => request('GET', `/api/v1/files/recordings${q ? `?${q}` : ''}`),
};
