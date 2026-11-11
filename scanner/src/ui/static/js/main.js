// Page shell: the module rail, the viewed module's tabs, the header (which module has the radio,
// this browser's volume, live audio and its listening settings, who is signed in), the page
// router and the store the radio pushes. The modules and their pages come from modules.js. A
// radio with a password shows its sign-in page first, and nothing else until that is done.
//
// Viewing a module does not move the radio: its pages stay readable while another module has it,
// under a notice that offers the switch.

import { store, subscribe, start } from './store.js';
import { api, onSignedOut } from './api.js';
import { h, icon, setText, setClass } from './dom.js';
import { MODULES } from './modules.js';
import { mode, giveRadio, changingMode, onModeChanging } from './mode.js';
import { player } from './audio/player.js';
import { setRecordingVolume } from './audio/recording.js';
import { tv } from './tv/player.js';
import { getPrefs, setPrefs, onPrefs } from './prefs.js';

const $ = id => document.getElementById(id);
// Page key (the address after #) -> its module and page.
const PAGES = new Map(MODULES.flatMap(m => m.pages.map(p => [p.key, { module: m, page: p }])));
const SCANNER = MODULES.find(m => m.mode === 'scanner');

// The module that has the radio.
const owner = () => MODULES.find(m => m.mode === mode()) || SCANNER;

// The mounted page: { key, module, view }.
let current = null;
// Opened with no address: the page moves to the radio's module once the radio says which.
let landing = !location.hash;

// The rail: the modes' modules at the top, the radio's own diagnostics, files and settings at
// the bottom. Module id -> its link and its "has the radio" dot.
const rail = new Map();
for (const m of MODULES) {
  const dot = h('span', { class: 'rail-dot', title: 'Has the radio', hidden: true });
  const a = h('a', { class: 'rail-item', href: `#${m.pages[0].key}`, title: m.title || null }, icon(m.icon, 22), h('span', { class: 'rail-label' }, dot, m.label));
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
  // The page's own layout: one that fills the screen scrolls its panes, not the page.
  document.body.dataset.view = hit.page.key;
  document.body.toggleAttribute('data-fill', !!hit.page.fill);
  current = { key: hit.page.key, module: hit.module, view: hit.page.view.mount(host) };
  nav();
  shell(store);
  if (store.status) current.view.update(store);
}

// The rail's mark, the module's name and its pages' tabs: the module's own diagnostics and
// settings set apart after the rest.
function nav() {
  const m = current.module;
  for (const [id, r] of rail) {
    setClass(r.a, 'active', id === m.id);
    if (id === m.id) r.a.setAttribute('aria-current', 'true');
    else r.a.removeAttribute('aria-current');
  }
  setText($('module-title'), m.title || m.label);
  const tabs = $('tabs');
  tabs.hidden = m.pages.length < 2;
  const firstTool = m.pages.find(p => p.tool);
  tabs.replaceChildren(...(tabs.hidden ? [] : m.pages.map(p => h('a', {
    href: `#${p.key}`, 'aria-current': p.key === current.key ? 'page' : null, text: p.label,
    class: [p.key === current.key ? 'active' : '', p === firstTool ? 'tool-first' : ''].filter(Boolean).join(' ') || null,
    title: p.tool ? `${m.label}'s own ${p.label.toLowerCase()}` : null,
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
    $('banner-switch').disabled = changingMode();
  }
  // On the radio's own module and on the tools: the way back to the scanner.
  const back = $('radio-back');
  back.hidden = !s.status || own === SCANNER || away;
  setText(back, `Return to ${SCANNER.label}`);
  back.disabled = changingMode();
}

// The radio to the viewed module, or back to the scanner (mode.js asks first and reports).
$('banner-switch').addEventListener('click', () => giveRadio(current.module.mode, current.module.label));
$('radio-back').addEventListener('click', () => giveRadio(SCANNER.mode, SCANNER.label));
onModeChanging(() => shell(store));

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
// The volume (live audio, recordings and the TV's sound, up to full scale), the leveller, the
// lanes apart on the two speakers and alerts-only listening are this browser's (prefs.js).
$('listen').addEventListener('click', () => {
  const on = !player.playing;
  setPrefs({ listening: on });
  if (on) player.start();
  else player.stop();
});
$('vol').addEventListener('input', e => {
  player.setVolume(e.target.value / 100);
  setRecordingVolume(e.target.value / 100);
  tv.setVolume(e.target.value / 100);
});
$('vol').addEventListener('change', e => setPrefs({ volume: e.target.value / 100 }));
$('norm').addEventListener('click', () => setPrefs({ normalize: !getPrefs().normalize }));
$('split').addEventListener('click', () => setPrefs({ split: !getPrefs().split }));
// The lanes play apart only where the site runs two: one lane alone plays on both speakers.
let twoLanes = false;
const applySplit = () => player.setSplit(getPrefs().split && twoLanes);
$('alerts-only').addEventListener('click', () => setPrefs({ alertsOnly: !getPrefs().alertsOnly }));
$('alert-window').addEventListener('change', e => setPrefs({ alertMinutes: Number(e.target.value) }));
onPrefs(p => {
  player.setVolume(p.volume);
  setRecordingVolume(p.volume);
  tv.setVolume(p.volume);
  player.setNormalize(p.normalize);
  applySplit();
  p.lanes.forEach((l, i) => player.setLaneVolume(i, l.muted ? 0 : l.volume));
  player.setAlertMode(p.alertsOnly, p.alertMinutes * 60000);
  if (document.activeElement !== $('vol')) $('vol').value = String(Math.round(p.volume * 100));
  $('vol').title = `Volume ${$('vol').value} %`;
  for (const [id, on] of [['split', p.split], ['norm', p.normalize], ['alerts-only', p.alertsOnly]]) {
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
  const two = ((s.traffic && s.traffic.channels) || []).length > 1;
  if (two !== twoLanes) {
    twoLanes = two;
    applySplit();
  }
  shell(s);
  if (current) current.view.update(s);
});
window.addEventListener('hashchange', () => {
  landing = false;
  if (current) route();
});

// A radio with a password, and this browser not signed in: its sign-in page, and nothing else.
function signIn() {
  document.body.dataset.signin = '';
  $('signin').hidden = false;
  const password = $('signin-password');
  password.focus();
  $('signin-form').addEventListener('submit', async e => {
    e.preventDefault();
    try {
      await api.signIn(password.value);
      location.reload();
    } catch (err) {
      setText($('signin-message'), err.message);
      password.select();
    }
  });
}

// Who is signed in here, with the way out. A radio with no password shows neither.
function account(me) {
  if (!me || me.via !== 'session') return;
  $('account').hidden = false;
  setText($('account-level'), me.level === 'admin' ? 'Administrator' : 'Listener');
  document.body.dataset.level = me.level;
  $('sign-out').addEventListener('click', async () => {
    try {
      await api.signOut();
    } finally {
      location.reload();
    }
  });
}

// The page starts once the radio said who this browser is to it. A radio that does not answer
// starts the page too: its header says "Not connected".
async function begin() {
  let me = null;
  try {
    me = await api.session();
  } catch { /* not reached: the store goes on trying */ }
  store.session = me;
  if (me && me.protected && !me.level) {
    signIn();
    return;
  }
  // Signed out elsewhere, or by a changed password: back to the sign-in page.
  onSignedOut(() => location.reload());
  let asked = false;
  subscribe(async s => {
    if (s.connected || asked || !me || !me.protected) return;
    asked = true;
    try {
      const now = await api.session();
      if (now.protected && !now.level) location.reload();
    } catch { /* the radio is away: the socket goes on trying */ }
    asked = false;
  });
  account(me);
  route();
  start();
}
begin();
