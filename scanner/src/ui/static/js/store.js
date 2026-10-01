// Shared page state: the status, polled every 2 s, and the connection. Pages subscribe and get
// the whole store on every change.

import { api } from './api.js';

export const store = { status: null, connected: false };

const subscribers = new Set();

export function subscribe(fn) {
  subscribers.add(fn);
  return () => subscribers.delete(fn);
}

function notify() {
  for (const fn of subscribers) fn(store);
}

let timer = null;

export async function refresh() {
  try {
    store.status = await api.status();
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
