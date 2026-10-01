// Shared page state: the status, the calls and the newest recordings, polled every 2 s, and the
// connection. Pages subscribe and get the whole store on every change.

import { api } from './api.js';

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

export function start() {
  refresh();
  clearInterval(timer);
  timer = setInterval(refresh, 2000);
}
