// Page shell: the header, the page router (#now, #activity, #systems, #diag, #settings) and the store.
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

// Live audio: the button starts and stops it (the server says which speaker each call is on).
$('listen').addEventListener('click', () => player.toggle());
player.onChange(st => {
  setText($('listen'), st.playing ? 'Stop' : 'Listen');
  $('listen').setAttribute('aria-pressed', String(st.playing));
  setText($('listen-state'), st.label);
  $('listen-state').title = st.detail;
});
subscribe(s => {
  header(s);
  if (current) current.page.update(s);
});
window.addEventListener('hashchange', route);
route();
start();
