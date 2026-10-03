// This browser's listening preferences, kept across page loads in localStorage (a private window
// or blocked storage keeps them for the page only): whether Listen was on, the volume, the
// leveller, each traffic lane's volume and mute, and alerts-only listening.

const KEY = 'scanner.listen';
const DEFAULTS = {
  // Listen was on: it starts again when the page loads.
  listening: false,
  // Every lane, 0 to 2.
  volume: 1,
  // Evens out loud and quiet talkers (ring.js).
  normalize: true,
  // Traffic 1 and Traffic 2.
  lanes: [{ volume: 1, muted: false }, { volume: 1, muted: false }],
  // Play only the talkgroups an alert tone opened, for `alertMinutes` (audio/gate.js).
  alertsOnly: false,
  alertMinutes: 2,
};

const listeners = new Set();
let prefs = load();

function load() {
  try {
    const saved = JSON.parse(localStorage.getItem(KEY) || '{}');
    const lanes = DEFAULTS.lanes.map((d, i) => ({ ...d, ...((saved.lanes || [])[i] || {}) }));
    return { ...DEFAULTS, ...saved, lanes };
  } catch {
    return { ...DEFAULTS, lanes: DEFAULTS.lanes.map(l => ({ ...l })) };
  }
}

export function getPrefs() {
  return prefs;
}

export function setPrefs(change) {
  prefs = { ...prefs, ...change };
  try {
    localStorage.setItem(KEY, JSON.stringify(prefs));
  } catch { /* kept for this page */ }
  for (const fn of listeners) fn(prefs);
}

// One lane's volume or mute (0: Traffic 1).
export function setLane(i, change) {
  setPrefs({ lanes: prefs.lanes.map((l, j) => (j === i ? { ...l, ...change } : l)) });
}

// fn(prefs) now and at each change.
export function onPrefs(fn) {
  listeners.add(fn);
  fn(prefs);
  return () => listeners.delete(fn);
}
