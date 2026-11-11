// The TV channels a scan named, as the TV pages list them, and what this browser last picked: the
// channel, and Watch's view.

const KEY = 'tv.channel';
const VIEW_KEY = 'tv.view';
// Watch's views in the page: the channels, the picture and the signal side by side, or the
// picture across the page with the channels as a strip below it.
export const VIEWS = ['small', 'large'];

// "19:47.1": RF channel 19's virtual channel 47.1.
export const channelKey = (rf, v) => `${rf}:${v.major}.${v.minor}`;

// Every virtual channel the scan named, by its number: { key, rf (the scan's row), st (its
// station), v }.
export function channelsOf(scan) {
  const out = [];
  for (const rf of scan ? scan.found : []) {
    const st = rf.station;
    if (!st || !st.channels) continue;
    for (const v of st.channels) out.push({ key: channelKey(rf.number, v), rf, st, v });
  }
  return out.sort((a, b) => a.v.major - b.v.major || a.v.minor - b.v.minor || a.rf.number - b.rf.number);
}

// This browser's last channel, kept for its next visit.
export function pickedChannel() {
  try {
    return localStorage.getItem(KEY);
  } catch {
    return null;
  }
}

export function pickChannel(key) {
  try {
    localStorage.setItem(KEY, key);
  } catch {
    // Storage refused (a private window): the pick lasts this visit only.
  }
}

export function pickedView() {
  try {
    const v = localStorage.getItem(VIEW_KEY);
    return VIEWS.includes(v) ? v : VIEWS[0];
  } catch {
    return VIEWS[0];
  }
}

export function pickView(view) {
  try {
    localStorage.setItem(VIEW_KEY, view);
  } catch {
    // As above.
  }
}
