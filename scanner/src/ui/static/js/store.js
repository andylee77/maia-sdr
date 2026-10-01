// Shared page state: the status, the calls and the newest recordings, polled every 2 s, and the
// connection. Pages subscribe and get the whole store on every change.

import { api, wsUrl } from './api.js';

export const store = { status: null, calls: null, recordings: null, connected: false };

const subscribers = new Set();

export function subscribe(fn) {
  subscribers.add(fn);
  return () => subscribers.delete(fn);
}

function notify() {
  for (const fn of subscribers) fn(store);
}

// Enough to cover the recent calls the Now page lists.
const RECORDINGS = 40;

let timer = null;

export async function refresh() {
  try {
    [store.status, store.calls, store.recordings] = await Promise.all([api.status(), api.calls(), api.recordings(RECORDINGS)]);
    store.connected = true;
  } catch (e) {
    store.connected = false;
  }
  notify();
}

// A refresh the moment a call opens or closes or a recording is saved (`/ws/events`), on top of
// the poll.
let kick = null;
function listen(backoff = 1000) {
  const ws = new WebSocket(wsUrl('/ws/events'));
  ws.onopen = () => { backoff = 1000; };
  ws.onmessage = () => {
    clearTimeout(kick);
    kick = setTimeout(refresh, 150);
  };
  ws.onclose = () => setTimeout(() => listen(Math.min(backoff * 2, 30000)), backoff);
}

export function start() {
  refresh();
  clearInterval(timer);
  timer = setInterval(refresh, 2000);
  listen();
}
