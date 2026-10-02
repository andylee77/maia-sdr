// Shared page state, pushed by the radio over /ws/live: the status (each second), the calls on the
// air and the newest closed ones, the traffic channels, the newest recordings and the systems.
// Pages subscribe and get the whole store on every change. A part is replaced, never changed in
// place, so a page can tell what changed by comparing it with what it drew.

import { api, wsUrl } from './api.js';

export const store = {
  status: null,
  // When the status and the traffic channels arrived (Date.now()): ages move on from there.
  statusAt: 0,
  calls: null,
  traffic: null,
  trafficAt: 0,
  recordings: null,
  systems: null,
  connected: false,
};

// The closed calls the radio keeps for the live site (`/api/v1/calls`), and as many recordings.
const RECENT = 100;

const subscribers = new Set();

export function subscribe(fn) {
  subscribers.add(fn);
  return () => subscribers.delete(fn);
}

function notify() {
  for (const fn of subscribers) fn(store);
}

function liveSite(status) {
  const live = status && status.live;
  return live && live.state === 'live' ? live.site.id : null;
}

// The site whose calls and recordings the store holds.
let callsOf = null;

async function readCalls() {
  const site = liveSite(store.status);
  callsOf = site;
  try {
    const [calls, recordings] = await Promise.all([api.calls(), api.recordings(RECENT, site)]);
    if (callsOf !== site) return;
    store.calls = calls;
    store.recordings = recordings;
    notify();
  } catch {
    // Read again with the next status.
    if (callsOf === site) callsOf = undefined;
  }
}

async function readRecordings() {
  try {
    store.recordings = await api.recordings(RECENT, callsOf);
    notify();
  } catch { /* read again at the next change */ }
}

async function readSystems() {
  try {
    store.systems = await api.systems();
    notify();
  } catch { /* read again at the next change */ }
}

function setStatus(status) {
  store.status = status;
  store.statusAt = Date.now();
  // Another site went live: its calls replace the last one's.
  if (liveSite(status) !== callsOf) readCalls();
}

function setTraffic(traffic) {
  store.traffic = traffic;
  store.trafficAt = Date.now();
  if (store.calls) store.calls = { open: traffic.open, recent: store.calls.recent };
}

// After an action: the status and the calls now, rather than at the next push.
export async function refresh() {
  try {
    setStatus(await api.status());
    notify();
  } catch { /* the push follows */ }
}

function receive(m) {
  switch (m.type) {
    case 'snapshot':
      callsOf = liveSite(m.status);
      store.calls = m.calls;
      setStatus(m.status);
      setTraffic(m.traffic);
      readRecordings();
      readSystems();
      break;
    case 'status':
      setStatus(m.status);
      break;
    case 'traffic':
      setTraffic(m.traffic);
      break;
    case 'call_opened':
    case 'call_closed': {
      const c = m.call;
      const v = store.calls || { open: [], recent: [] };
      const open = v.open.filter(x => x.call !== c.call);
      const recent = v.recent.filter(x => x.call !== c.call);
      if (m.type === 'call_opened') open.push(c);
      else recent.unshift(c);
      store.calls = { open, recent: recent.slice(0, RECENT) };
      break;
    }
    case 'recording': {
      const r = m.recording;
      const v = store.recordings || { total: 0, items: [] };
      store.recordings = { ...v, items: [r, ...v.items.filter(x => x.id !== r.id)].slice(0, RECENT) };
      break;
    }
    case 'changed':
      // Systems carry the names the calls show.
      if (m.what === 'systems') {
        readSystems();
        readCalls();
      } else if (m.what === 'recordings') {
        readRecordings();
      }
      return;
    default:
      return;
  }
  notify();
}

function connect(backoff = 1000) {
  const ws = new WebSocket(wsUrl('/ws/live'));
  ws.onopen = () => {
    backoff = 1000;
    store.connected = true;
  };
  ws.onmessage = e => {
    try {
      receive(JSON.parse(e.data));
    } catch { /* not ours */ }
  };
  ws.onclose = () => {
    store.connected = false;
    notify();
    setTimeout(() => connect(Math.min(backoff * 2, 30000)), backoff);
  };
}

export function start() {
  connect();
}
