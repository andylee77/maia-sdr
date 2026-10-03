// Page shell: the header with live audio and this browser's listening settings, the page router
// (#now, #activity, #systems, #diag, #settings) and the store the radio pushes.
// Pages are modules exporting mount(el) -> { update(store), unmount() }.

import { store, subscribe, start } from './store.js';
import { setText, setClass } from './dom.js';
import { mhz } from './format.js';
import * as now from './views/now.js';
import * as activity from './views/activity.js';
import * as systems from './views/systems.js';
import * as diag from './views/diagnostics.js';
import * as settings from './views/settings.js';
import { player } from './audio/player.js';
import { getPrefs, setPrefs, onPrefs } from './prefs.js';

const PAGES = { now, activity, systems, diag, settings };
const $ = id => document.getElementById(id);

let current = null;

function route() {
  const name = (location.hash || '#now').slice(1);
  const key = PAGES[name] ? name : 'now';
  if (current && current.name === key) return;
  if (current && current.page.unmount) current.page.unmount();
  const host = $('view');
  host.replaceChildren();
  // The page's own layout (the Now page fills the screen and scrolls only its calls).
  document.body.dataset.view = key;
  current = { name: key, page: PAGES[key].mount(host) };
  for (const a of document.querySelectorAll('#tabs a')) {
    setClass(a, 'active', a.dataset.view === key);
    if (a.dataset.view === key) a.setAttribute('aria-current', 'page');
    else a.removeAttribute('aria-current');
  }
  if (store.status) current.page.update(store);
}

function header(s) {
  setClass($('conn'), 'ok', s.connected);
  const live = s.status && s.status.live;
  if (!s.connected) {
    setText($('top-site-text'), 'not connected');
  } else if (!live || live.state === 'no_site') {
    setText($('top-site-text'), 'no site');
  } else if (live.state === 'switching') {
    setText($('top-site-text'), `switching to ${live.to}…`);
  } else if (live.state === 'scanning') {
    setText($('top-site-text'), 'scanning…');
  } else {
    setText($('top-site-text'), `${live.site.label} · ${mhz(live.site.control.freq_hz)}`);
  }
  setClass($('top-site-dot'), 'ok', !!live && live.state === 'live');
}

// The talkgroup's name from the calls the store holds.
function tgName(tg) {
  const c = store.calls && [...store.calls.open, ...store.calls.recent].find(x => x.tg === tg && x.tg_name);
  return c ? c.tg_name : `TG ${tg}`;
}

// "TG 300 1:45": the talkgroups an alert opened, with the time each has left.
function openedText(open) {
  return open.map(([tg, ms]) => {
    const s = Math.ceil(ms / 1000);
    return `${tgName(tg)} ${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  }).join(', ');
}

// Live audio: the button starts and stops it, and a page load starts it again when it was on.
// The volume, the leveller and alerts-only listening are this browser's (prefs.js); the server
// says which speaker each call is on.
$('listen').addEventListener('click', () => {
  const on = !player.playing;
  setPrefs({ listening: on });
  if (on) player.start();
  else player.stop();
});
$('vol').addEventListener('input', e => player.setVolume(e.target.value / 100));
$('vol').addEventListener('change', e => setPrefs({ volume: e.target.value / 100 }));
$('norm').addEventListener('click', () => setPrefs({ normalize: !getPrefs().normalize }));
$('alerts-only').addEventListener('click', () => setPrefs({ alertsOnly: !getPrefs().alertsOnly }));
$('alert-window').addEventListener('change', e => setPrefs({ alertMinutes: Number(e.target.value) }));
onPrefs(p => {
  player.setVolume(p.volume);
  player.setNormalize(p.normalize);
  p.lanes.forEach((l, i) => player.setLaneVolume(i, l.muted ? 0 : l.volume));
  player.setAlertMode(p.alertsOnly, p.alertMinutes * 60000);
  if (document.activeElement !== $('vol')) $('vol').value = String(Math.round(p.volume * 100));
  $('vol').title = `Volume ${Math.round(p.volume * 100)} %`;
  for (const [id, on] of [['norm', p.normalize], ['alerts-only', p.alertsOnly]]) {
    setClass($(id), 'on', on);
    $(id).setAttribute('aria-pressed', String(on));
  }
  $('alert-window').value = String(p.alertMinutes);
  $('alert-window').hidden = !p.alertsOnly;
});
player.onChange(st => {
  setText($('listen'), st.playing ? 'Stop' : 'Listen');
  $('listen').setAttribute('aria-pressed', String(st.playing));
  const open = st.playing && st.open && st.open.length ? openedText(st.open) : '';
  setText($('listen-state'), open ? `Alert: ${open}` : st.label);
  setClass($('listen-state'), 'alerting', !!open);
  setClass($('listen-state'), 'waiting', !!st.waiting);
  $('listen-state').title = st.detail;
});
// A browser starts audio only after a click on the page: Listen restored at load waits for one.
for (const kind of ['pointerdown', 'keydown']) document.addEventListener(kind, () => player.resume(), { capture: true });
if (getPrefs().listening) player.start();
subscribe(s => {
  header(s);
  if (current) current.page.update(s);
});
window.addEventListener('hashchange', route);
route();
start();
