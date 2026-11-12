// What Data's pages share: a device's name as the pages show it, and a reading in words, its
// temperature in the unit the radio's Settings picks.

import { temperature } from '../format.js';
import { store } from '../store.js';

// "Oregon Scientific v1 · id 3 · channel 1".
export function deviceName(d) {
  return `${d.model} · id ${d.id}${d.channel === null ? '' : ` · channel ${d.channel}`}`;
}

// What a reading says: "58.1 °F", with "battery low" when it is.
export function readingText(r) {
  const parts = [];
  if (r.temperature_c !== null) parts.push(temperature(r.temperature_c, store.units.temperature));
  if (r.battery_low) parts.push('battery low');
  return parts.join(' · ');
}
