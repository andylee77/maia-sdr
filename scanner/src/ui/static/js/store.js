// Shared page state, pushed by the radio over /ws/live: the status (each second), the calls on the
// air and the newest closed ones (each with its recording and alert tones once known), the
// traffic channels, the newest recordings, the alert tones heard and the systems.
// Pages subscribe and get the whole store on every change. A part is replaced, never changed in
// place, so a page can tell what changed by comparing it with what it drew.
//
// What only some pages show (the spectrum, the event log, the radio's readback, the receive
// window, the crystal) comes over the same socket while a page wants it (`want`).

import { api, wsUrl } from './api.js';

export const store = {
  status: null,
  // When the status and the traffic channels arrived (Date.now()): ages move on from there.
  statusAt: 0,
  calls: null,
  traffic: null,
  trafficAt: 0,
  recordings: null,
  // Alert tones heard since the page loaded, newest first.
  alerts: [],
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

// A closed call learns its recording and alert tones after it closed.
function patchCall(id, change) {
  if (!store.calls) return;
  const recent = store.calls.recent.map(x => (x.call === id ? { ...x, ...change } : x));
  store.calls = { open: store.calls.open, recent };
}

function setTraffic(traffic) {
  store.traffic = traffic;
  store.trafficAt = Date.now();
  if (store.calls) store.calls = { open: traffic.open, recent: store.calls.recent };
}

// Pages' wants: want({spectrum: 1024}, fn) and fn gets each `spectrum` message until the
// returned function is called. `events: {after, routine}` is the event log past `after`; its
// page moves `after` on as lines come, and gets {type: 'restart'} when the scanner restarted
// (the log starts again).
const wanting = new Map();
let nextWant = 1;
let socket = null;
const FEEDS = ['spectrum', 'events', 'radio', 'window', 'crystal'];

export function want(wants, onMessage) {
  const id = nextWant++;
  wanting.set(id, { wants, onMessage });
  sendWants();
  return () => {
    wanting.delete(id);
    sendWants();
  };
}

// Every page's wants as one: the most spectrum bins, the event log of the first page that
// wants it, and the rest if any page wants them.
function sendWants() {
  if (!socket || socket.readyState !== WebSocket.OPEN) return;
  const out = { type: 'subscribe' };
  for (const { wants } of wanting.values()) {
    if (wants.spectrum) out.spectrum = Math.max(out.spectrum || 0, wants.spectrum);
    if (wants.events && !out.events) out.events = wants.events;
    for (const k of ['radio', 'window', 'crystal']) if (wants[k]) out[k] = true;
  }
  socket.send(JSON.stringify(out));
}

function feed(m) {
  for (const { wants, onMessage } of wanting.values()) if (wants[m.type]) onMessage(m);
}

// The scanner restarted (a snapshot with less uptime than the last status): the event log
// numbers its lines afresh.
function restarted() {
  for (const { wants, onMessage } of wanting.values()) {
    if (wants.events) {
      wants.events.after = 0;
      onMessage({ type: 'restart' });
    }
  }
  sendWants();
}

// After an action: the status and the calls now, rather than at the next push.
export async function refresh() {
  try {
    setStatus(await api.status());
    notify();
  } catch { /* the push follows */ }
}

function receive(m) {
  if (FEEDS.includes(m.type)) {
    feed(m);
    return;
  }
  switch (m.type) {
    case 'snapshot':
      if (store.status && m.status.uptime_s < store.status.uptime_s) restarted();
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
      const isNew = !v.items.some(x => x.id === r.id);
      store.recordings = { ...v, total: v.total + (isNew ? 1 : 0), items: [r, ...v.items.filter(x => x.id !== r.id)].slice(0, RECENT) };
      patchCall(r.id, { recording: r });
      break;
    }
    case 'alert':
      patchCall(m.call, { alerts: m.alerts });
      store.alerts = [...m.alerts, ...store.alerts].slice(0, RECENT);
      break;
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
  const ws = socket = new WebSocket(wsUrl('/ws/live'));
  ws.onopen = () => {
    backoff = 1000;
    store.connected = true;
    sendWants();
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
