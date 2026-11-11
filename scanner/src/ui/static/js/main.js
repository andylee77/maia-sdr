// Page shell: the module rail, the viewed module's tabs, the header (which module has the radio,
// live audio and this browser's listening settings), the page router and the store the radio
// pushes. The modules and their pages come from modules.js.
//
// Viewing a module does not move the radio: its pages stay readable while another module has it,
// under a notice that offers the switch.

import { store, subscribe, start, refresh } from './store.js';
import { h, icon, setText, setClass, toast } from './dom.js';
import { api } from './api.js';
import { MODULES } from './modules.js';
import { player } from './audio/player.js';
import { setRecordingVolume } from './audio/recording.js';
import { getPrefs, setPrefs, onPrefs } from './prefs.js';

const $ = id => document.getElementById(id);
// Page key (the address after #) -> its module and page.
const PAGES = new Map(MODULES.flatMap(m => m.pages.map(p => [p.key, { module: m, page: p }])));
const SCANNER = MODULES.find(m => m.mode === 'scanner');

const mode = () => (store.status && store.status.mode) || 'scanner';
// The module that has the radio.
const owner = () => MODULES.find(m => m.mode === mode()) || SCANNER;

// The mounted page: { key, module, view }.
let current = null;
// Opened with no address: the page moves to the radio's module once the radio says which.
let landing = !location.hash;
// A change of the radio's module is under way.
let switching = false;

// The rail: the modes' modules at the top, the shared tools at the bottom. Module id -> its
// link and its "has the radio" dot.
const rail = new Map();
for (const m of MODULES) {
  const dot = h('span', { class: 'rail-dot', title: 'Has the radio', hidden: true });
  const a = h('a', { class: 'rail-item', href: `#${m.pages[0].key}` }, icon(m.icon, 22), h('span', { class: 'rail-label' }, dot, m.label));
  rail.set(m.id, { a, dot });
  $(m.mode ? 'rail-modules' : 'rail-tools').append(a);
}

function route() {
  let hit = PAGES.get((location.hash || '').slice(1));
  if (!hit) {
    hit = PAGES.get(owner().pages[0].key);
    history.replaceState(null, '', `#${hit.page.key}`);
  }
  if (current && current.key === hit.page.key) return;
  if (current && current.view.unmount) current.view.unmount();
  const host = $('view');
  host.replaceChildren();
  // The page's own layout (Now fills the screen and scrolls its panes).
  document.body.dataset.view = hit.page.key;
  current = { key: hit.page.key, module: hit.module, view: hit.page.view.mount(host) };
  nav();
  shell(store);
  if (store.status) current.view.update(store);
}

// The rail's mark, the module's name and its pages' tabs.
function nav() {
  const m = current.module;
  for (const [id, r] of rail) {
    setClass(r.a, 'active', id === m.id);
    if (id === m.id) r.a.setAttribute('aria-current', 'true');
    else r.a.removeAttribute('aria-current');
  }
  setText($('module-title'), m.label);
  const tabs = $('tabs');
  tabs.hidden = m.pages.length < 2;
  tabs.replaceChildren(...(tabs.hidden ? [] : m.pages.map(p => h('a', {
    href: `#${p.key}`, class: p.key === current.key ? 'active' : null, 'aria-current': p.key === current.key ? 'page' : null, text: p.label,
  }))));
}

// What follows the radio: the header's line, the rail's dot, the notice on a module the radio is
// not on, and live audio's controls (the scanner's).
function shell(s) {
  const own = owner();
  const known = s.connected && !!s.status;
  setText($('radio-text'), !s.connected ? 'Not connected' : !s.status ? 'Connecting…' : `Radio on ${own.label} · ${own.line(s)}`);
  setClass($('radio-dot'), 'ok', known && own.working(s));
  for (const [id, r] of rail) r.dot.hidden = !(known && id === own.id);
  $('listen-box').hidden = own !== SCANNER;
  const viewed = current && current.module;
  const away = !!s.status && !!viewed && !!viewed.mode && viewed !== own;
  $('banner').hidden = !away;
  if (away) {
    setText($('banner-text'), `The radio is on ${own.label}. ${viewed.away}`);
    setText($('banner-switch'), `Switch the radio to ${viewed.label}`);
    $('banner-switch').disabled = switching;
  }
  // On the radio's own module and on the tools: the way back to the scanner.
  const back = $('radio-back');
  back.hidden = !s.status || own === SCANNER || away;
  setText(back, `Return to ${SCANNER.label}`);
  back.disabled = switching;
}

// Give the radio to module `m`. A module has the radio to itself, so leaving the scanner stops
// its calls until it is back. The change returns once the radio is the new module's.
async function giveRadio(m) {
  if (switching || m === owner()) return;
  if (owner() === SCANNER && !confirm(`Switch the radio to ${m.label}? The scanner stops following calls until it is switched back.`)) return;
  switching = true;
  shell(store);
  try {
    await api.setMode(m.mode);
    await refresh();
    toast(`The radio is on ${m.label}`);
  } catch (err) {
    toast(err.message, true);
  } finally {
    switching = false;
    shell(store);
  }
}
$('banner-switch').addEventListener('click', () => giveRadio(current.module));
$('radio-back').addEventListener('click', () => giveRadio(SCANNER));

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
// The volume (live audio and recordings, up to full scale), the leveller and alerts-only listening
// are this browser's (prefs.js); the server says which speaker each call is on.
$('listen').addEventListener('click', () => {
  const on = !player.playing;
  setPrefs({ listening: on });
  if (on) player.start();
  else player.stop();
});
$('vol').addEventListener('input', e => {
  player.setVolume(e.target.value / 100);
  setRecordingVolume(e.target.value / 100);
});
$('vol').addEventListener('change', e => setPrefs({ volume: e.target.value / 100 }));
$('norm').addEventListener('click', () => setPrefs({ normalize: !getPrefs().normalize }));
$('alerts-only').addEventListener('click', () => setPrefs({ alertsOnly: !getPrefs().alertsOnly }));
$('alert-window').addEventListener('change', e => setPrefs({ alertMinutes: Number(e.target.value) }));
onPrefs(p => {
  player.setVolume(p.volume);
  setRecordingVolume(p.volume);
  player.setNormalize(p.normalize);
  p.lanes.forEach((l, i) => player.setLaneVolume(i, l.muted ? 0 : l.volume));
  player.setAlertMode(p.alertsOnly, p.alertMinutes * 60000);
  if (document.activeElement !== $('vol')) $('vol').value = String(Math.round(p.volume * 100));
  $('vol').title = `Volume ${$('vol').value} %`;
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
  if (landing && s.status) {
    landing = false;
    if (current.module.mode && current.module !== owner()) location.hash = `#${owner().pages[0].key}`;
  }
  shell(s);
  if (current) current.view.update(s);
});
window.addEventListener('hashchange', () => {
  landing = false;
  route();
});
route();
start();
