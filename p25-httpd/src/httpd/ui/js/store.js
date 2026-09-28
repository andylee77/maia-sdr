// The single state object every view renders from, plus the polling
// that keeps it fresh.
//
//   store.state    last GET /api/ui/state   (polled 1 Hz, 5 s when hidden)
//   store.calls    last GET /api/ui/calls   (refetched when calls_rev moves)
//   store.settings last GET /api/ui/settings (refetched when settings_rev
//                  moves; change 063: the speaker groups live there)
//   store.conn     link health to the radio
//
// A /ws/events connection (when enabled) only "kicks" an early state
// poll on grants / call ends / saved recordings, so a new call shows up
// within ~0.2 s instead of up to 1 s. All data still comes from the
// two documents above.

import { api, openEvents } from './api.js';

const POLL_VISIBLE_MS = 1000;
const POLL_HIDDEN_MS = 5000;
const CALLS_REFRESH_MS = 30000;
// Change 057: TRF_VOICE_END = end of a transmission (the call card
// switches to "Ended" at once).
const KICK_TYPES = new Set(['GRP_VCH_GRANT', 'recording_saved', 'TRF_HDU', 'TRF_TDULC_CALL_TERM', 'TRF_VOICE_END']);

export const store = {
  state: null,
  calls: null,
  settings: null,
  conn: { ok: false, lastOkAt: 0, error: null, rttMs: 0, events: false },
  // Board clock minus browser clock, from the last state poll.
  boardOffsetMs: 0,
  receivedAt: 0,
  prefs: loadPrefs(),
};

const listeners = new Set();

// fn(kind, store) with kind in 'state' | 'calls' | 'settings' | 'conn' | 'prefs'.
export function subscribe(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

function emit(kind) {
  for (const fn of listeners) {
    try { fn(kind, store); } catch (e) { console.error('view update failed', e); }
  }
}

// Board time now, extrapolated from the last poll.
export function boardNow() {
  return Date.now() + store.boardOffsetMs;
}

// ── Preferences (per browser) ───────────────────────────────────
function loadPrefs() {
  // callsSite (change 073): whose calls Recent calls lists: '' = the
  // active site, a site name, 'all', or '-' (kept before sites were).
  const d = { showNotFollowed: false, normalize: true, liveEvents: true, autoClock: true, theme: 'auto', callLimit: 40, callsSite: '' };
  try { return Object.assign(d, JSON.parse(localStorage.getItem('p25ui.prefs') || '{}')); } catch { return d; }
}

export function setPref(key, value) {
  store.prefs[key] = value;
  try { localStorage.setItem('p25ui.prefs', JSON.stringify(store.prefs)); } catch { /* private mode */ }
  emit('prefs');
}

// ── Polling ─────────────────────────────────────────────────────
let pollTimer = null;
let inFlight = false;
let lastCallsRev = null;
let lastCallsAt = 0;
let callsInFlight = false;
let lastSettingsRev = null;

export async function refreshSettings() {
  try {
    store.settings = await api.settings();
    emit('settings');
  } catch (e) {
    console.warn('settings refresh failed', e);
  }
}

export async function refreshState() {
  if (inFlight) return;
  inFlight = true;
  const t0 = performance.now();
  try {
    const s = await api.state();
    const rtt = performance.now() - t0;
    store.state = s;
    store.receivedAt = Date.now();
    store.boardOffsetMs = s.now_unix_ms + rtt / 2 - store.receivedAt;
    store.conn = Object.assign(store.conn, { ok: true, lastOkAt: Date.now(), error: null, rttMs: Math.round(rtt) });
    emit('state');
    if (s.calls_rev !== lastCallsRev || Date.now() - lastCallsAt > CALLS_REFRESH_MS) {
      refreshCalls();
    }
    if (s.settings_rev !== lastSettingsRev) {
      lastSettingsRev = s.settings_rev;
      refreshSettings();
    }
  } catch (e) {
    store.conn = Object.assign(store.conn, { ok: false, error: e.message || String(e) });
    emit('conn');
  } finally {
    inFlight = false;
  }
}

export async function refreshCalls() {
  if (callsInFlight) return;
  callsInFlight = true;
  try {
    const c = await api.calls(store.prefs.callLimit, store.prefs.showNotFollowed, store.prefs.callsSite);
    store.calls = c;
    lastCallsRev = c.calls_rev;
    lastCallsAt = Date.now();
    emit('calls');
  } catch (e) {
    console.warn('calls refresh failed', e);
  } finally {
    callsInFlight = false;
  }
}

function schedule(delay) {
  clearTimeout(pollTimer);
  pollTimer = setTimeout(tick, delay);
}

async function tick() {
  await refreshState();
  schedule(document.hidden ? POLL_HIDDEN_MS : POLL_VISIBLE_MS);
}

// Poll sooner than scheduled (debounced), e.g. after an event or an edit.
let kickTimer = null;
export function kick(delay = 150) {
  clearTimeout(kickTimer);
  kickTimer = setTimeout(() => { refreshState(); }, delay);
}

// ── Events socket (optional push) ───────────────────────────────
let closeEvents = null;

function syncEvents() {
  const want = store.prefs.liveEvents && !document.hidden;
  if (want && !closeEvents) {
    closeEvents = openEvents(evt => {
      if (evt.event_type === 'recording_saved') {
        lastCallsRev = null; // force the list refresh
        kick(50);
      } else if (KICK_TYPES.has(evt.event_type)) {
        kick();
      }
    }, connected => {
      store.conn.events = connected;
      emit('conn');
    });
  } else if (!want && closeEvents) {
    closeEvents();
    closeEvents = null;
    store.conn.events = false;
    emit('conn');
  }
}

export function start() {
  document.addEventListener('visibilitychange', () => {
    syncEvents();
    if (!document.hidden) schedule(0);
  });
  subscribe(kind => {
    if (kind === 'prefs') {
      syncEvents();
      lastCallsRev = null;
      refreshCalls();
    }
  });
  syncEvents();
  schedule(0);
}
