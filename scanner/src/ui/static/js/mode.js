// The unit's mode: which one has the radio, and giving the radio to another. A mode has the radio
// to itself, so leaving the scanner stops its calls until it is back: that is asked first. The
// shell (main.js) offers the change on a module the radio is not on, and a page does it when its
// work needs the radio (Watch, before it plays a channel).

import { store, refresh } from './store.js';
import { api } from './api.js';
import { toast } from './dom.js';

export const mode = () => (store.status && store.status.mode) || 'scanner';

let changing = false;
const listeners = new Set();

// A change of mode is under way.
export const changingMode = () => changing;

// fn() when a change starts and when it ends; returns its removal.
export function onModeChanging(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

function emit() {
  for (const fn of listeners) fn();
}

// Give the radio to mode `to`, called `label` on screen. True once the radio is its (or was
// already); false when the user said no, the radio refused, or another change is under way.
export async function giveRadio(to, label) {
  if (mode() === to) return true;
  if (changing) return false;
  if (mode() === 'scanner' && !confirm(`Switch the radio to ${label}? The scanner stops following calls until it is switched back.`)) return false;
  changing = true;
  emit();
  try {
    await api.setMode(to);
    await refresh();
    toast(`The radio is on ${label}`);
    return true;
  } catch (e) {
    toast(e.message, true);
    return false;
  } finally {
    changing = false;
    emit();
  }
}
