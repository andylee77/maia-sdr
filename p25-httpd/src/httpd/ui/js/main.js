// Boot: theme, header, view router (#now #radio #diag #settings),
// clock sync, live-audio button. Views are modules exporting
// { mount(el) -> { update(kind, store), unmount() } }.

import { store, subscribe, start, kick, setPref } from './store.js';
import { api } from './api.js';
import { setText, setClass, toast } from './dom.js';
import { mhz } from './format.js';
import { player, routeFromSettings } from './audio/player.js';
import * as nowView from './views/now.js';
import * as radioView from './views/radio.js';
import * as diagView from './views/diagnostics.js';
import * as settingsView from './views/settings.js';
import * as systemsView from './views/systems.js';

const VIEWS = { now: nowView, radio: radioView, systems: systemsView, diag: diagView, settings: settingsView };
const $ = id => document.getElementById(id);

let current = null; // { name, inst }

function route() {
  const name = (location.hash || '#now').slice(1);
  const key = VIEWS[name] ? name : 'now';
  if (current && current.name === key) return;
  if (current && current.inst.unmount) current.inst.unmount();
  const host = $('view');
  host.replaceChildren();
  current = { name: key, inst: VIEWS[key].mount(host) };
  for (const a of document.querySelectorAll('#tabs a')) {
    setClass(a, 'active', a.dataset.view === key);
    if (a.dataset.view === key) a.setAttribute('aria-current', 'page');
    else a.removeAttribute('aria-current');
  }
  if (store.state) current.inst.update('state', store);
  if (store.calls) current.inst.update('calls', store);
  window.scrollTo(0, 0);
}

function applyTheme() {
  const t = store.prefs.theme;
  if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t;
  else delete document.documentElement.dataset.theme;
}

function renderHeader() {
  const s = store.state;
  const dot = $('top-site-dot');
  if (s) {
    const site = s.site;
    const name = site.label || site.name || 'site';
    const nac = site.nac ? ' · NAC ' + site.nac : '';
    setText($('top-site-text'), name + nac + ' · CC ' + mhz(site.cc_freq_hz));
    setClass(dot, 'ok', site.health === 'ok');
    setClass(dot, 'warn', site.health === 'stale');
    setClass(dot, 'bad', site.health === 'searching');
    $('top-site').title = 'Control channel ' + site.health + (site.tsbk_per_s != null ? ' · ' + site.tsbk_per_s + ' TSBK/s' : '');
  }
  const conn = $('conn');
  if (store.conn.ok) {
    setText(conn, s ? s.build : '');
    setClass(conn, 'bad', false);
  } else {
    setText(conn, store.conn.lastOkAt ? 'radio unreachable' : 'connecting…');
    setClass(conn, 'bad', !!store.conn.lastOkAt);
  }
}

// The board clock is often never set (air-gapped). Offer — or, with
// the "auto" preference, do — a one-shot sync from this browser.
let clockSyncTried = false;
async function syncClock(manual) {
  try {
    await api.setTime(Date.now());
    toast('Radio clock set from this browser');
    kick(100);
  } catch (e) {
    if (manual) toast('Clock sync failed: ' + e.message, true);
  }
}

function renderBanner() {
  const s = store.state;
  const b = $('banner');
  const skew = s ? Math.abs(store.boardOffsetMs) : 0;
  // Change 067: with the clock source "site" the radio follows the
  // control channel's time on purpose (a replay may be months off this
  // browser); only an unset clock is flagged, and the browser never
  // overrides it. Browser sync is for "manual" / an NTP radio offline.
  const src = s && s.site ? s.site.clock_source : 'manual';
  const followsSite = src === 'site' && !!(s && s.site.site_time);
  const bad = s && (!s.clock_valid || (skew > 120000 && src !== 'site'));
  if (bad && !followsSite && store.prefs.autoClock && !clockSyncTried) {
    clockSyncTried = true;
    syncClock(false);
  }
  if (!bad) { b.hidden = true; return; }
  if (b.hidden) {
    b.replaceChildren(
      document.createTextNode(s.clock_valid
        ? 'The radio clock is off by ' + Math.round(skew / 1000) + ' s. Call times are shown relative to the radio.'
        : 'The radio clock is not set, so call times are shown as ages only.'),
    );
    const btn = document.createElement('button');
    btn.className = 'btn small';
    btn.textContent = 'Set from this browser';
    btn.onclick = () => syncClock(true);
    b.appendChild(btn);
    b.hidden = false;
  }
}

// Change 062: volume (per browser, 0-200 %). Change 063: the speaker
// routing comes from the radio's talkgroup groups (store.settings).
function applyAudioPrefs() {
  const vol = store.prefs.volume ?? 1;
  player.setVolume(vol);
  // Change 067: volume normalization, on unless switched off.
  const norm = store.prefs.normalize !== false;
  player.setNormalize(norm);
  const nb = $('norm-btn');
  if (nb) { nb.classList.toggle('on', norm); nb.setAttribute('aria-pressed', norm ? 'true' : 'false'); }
  const slider = $('vol');
  if (slider && document.activeElement !== slider) slider.value = Math.round(vol * 100);
  if (slider) slider.title = 'Volume ' + Math.round(vol * 100) + '%';
}

function bindListen() {
  const btn = $('listen-btn');
  const slider = $('vol');
  slider.addEventListener('input', () => {
    player.setVolume(slider.value / 100);
    slider.title = 'Volume ' + slider.value + '%';
  });
  slider.addEventListener('change', () => setPref('volume', slider.value / 100));
  $('norm-btn').addEventListener('click', () => setPref('normalize', store.prefs.normalize === false));
  applyAudioPrefs();
  btn.addEventListener('click', () => player.toggle());
  player.onChange(st => {
    btn.setAttribute('aria-pressed', st.playing ? 'true' : 'false');
    btn.textContent = st.playing ? (st.label || 'Listening') : 'Listen';
    btn.title = st.detail || 'Play the live traffic audio';
  });
}

function boot() {
  applyTheme();
  bindListen();
  window.addEventListener('hashchange', route);
  subscribe((kind, s) => {
    if (kind === 'prefs') { applyTheme(); applyAudioPrefs(); }
    if (kind === 'settings') player.setRoute(routeFromSettings(s.settings));
    if (kind === 'state' || kind === 'conn') { renderHeader(); renderBanner(); }
    if (current && current.inst.update) current.inst.update(kind, s);
  });
  route();
  start();
}

boot();
