// The unit's modules: the one table the page shell (main.js) builds the rail, each module's tabs
// and the router from. A module with a `mode` is one of the unit's modes: it has the radio to
// itself while the unit is in it, and its pages stay readable while another module has it. A
// module without one is the radio's own: what every mode shares (its `title` heads the page).
//
// A page is a view exporting mount(el) -> { update(store), unmount() }; one with `fill` takes the
// whole screen and scrolls its own panes. A module's own diagnostics and settings are pages of
// it like any other, marked `tool`: the shell sets them apart at the end of its tabs. A new
// module is an entry here, its views, and its mode in the daemon.

import { mhz } from './format.js';
import * as now from './views/now.js';
import * as activity from './views/activity.js';
import * as systems from './views/systems.js';
import * as aliases from './views/aliases.js';
import * as channels from './views/channels.js';
import * as watch from './views/watch.js';
import * as scannerDiag from './views/scanner_diag.js';
import * as scannerSettings from './views/scanner_settings.js';
import * as tvDiag from './views/tv_diag.js';
import * as dataScan from './views/data_scan.js';
import * as dataCaptures from './views/data_captures.js';
import * as dataDevices from './views/data_devices.js';
import * as analogListen from './views/analog_listen.js';
import * as aircraftList from './views/aircraft.js';
import * as aircraftMap from './views/aircraft_map.js';
import * as diag from './views/diagnostics.js';
import * as files from './views/files.js';
import * as settings from './views/settings.js';

// The scanner's line in the header: the live site, or what the radio does instead.
function scannerLine(s) {
  const live = s.status.live;
  if (!live || live.state === 'no_site') return 'no site';
  if (live.state === 'switching') return `switching to ${live.to}…`;
  if (live.state === 'scanning') return 'scanning for systems…';
  if (live.state === 'away') return 'changing…';
  return `${live.site.label} · ${mhz(live.site.control.freq_hz)}`;
}

// TV's line: the TV scan running, or what the last one found (frequencies with a signal, and
// the channels on them).
function tvLine(s) {
  const a = s.atsc;
  if (a && a.state === 'sweeping') {
    return a.identifying != null ? `scan, naming ${a.identified + 1} of ${a.to_identify}` : `scan, window ${a.step} of ${a.steps}`;
  }
  if (!a || !a.found.length) return 'no channels yet';
  const t = channels.tally(a);
  const freq = `${t.frequencies} frequenc${t.frequencies === 1 ? 'y' : 'ies'}`;
  return t.channels ? `${freq} · ${t.channels} channel${t.channels === 1 ? '' : 's'}` : freq;
}

// Data's line: the window it listens to. It listens only while it has the radio, and the
// scanner follows no call meanwhile.
function dataLine(s) {
  const d = s.status.data;
  if (!d || d.state === 'off') return 'not listening';
  if (d.state === 'starting') return 'starting…';
  if (d.state === 'error') return 'not listening: an error';
  return `${d.window} window · Scanner paused`;
}

// The analog receiver's line: what it is tuned to. It listens only while it has the radio.
const ANALOG_KIND = { wfm: 'WFM', nfm: 'NFM', am: 'AM' };
function analogLine(s) {
  const a = s.status.analog;
  if (!a || a.state === 'off') return 'not listening';
  if (a.state === 'starting') return 'starting…';
  if (a.state === 'error') return 'not listening: an error';
  return `${mhz(a.freq_hz)} ${ANALOG_KIND[a.demod]} · Scanner paused`;
}

// The Aircraft module's line: how many aircraft it hears. It listens only while it has the radio.
function aircraftLine(s) {
  const a = s.status.adsb;
  if (!a || a.state === 'off') return 'not listening';
  if (a.state === 'starting') return 'starting…';
  if (a.state === 'error') return 'not listening: an error';
  const where = a.link === 'both' ? '1090 and 978 MHz' : `${a.link} MHz`;
  const rate = Math.round(a.messages_per_s);
  return `${where} · ${a.aircraft} aircraft · ${rate} ${rate === 1 ? 'message' : 'messages'} a second · Scanner paused`;
}

export const MODULES = [
  {
    id: 'scanner',
    mode: 'scanner',
    label: 'Scanner',
    icon: 'tower',
    pages: [
      { key: 'now', label: 'Now', view: now, fill: true },
      { key: 'activity', label: 'Activity', view: activity, fill: true },
      { key: 'systems', label: 'Systems', view: systems, fill: true },
      { key: 'aliases', label: 'Aliases', view: aliases, fill: true },
      { key: 'scanner-diag', label: 'Diagnostics', view: scannerDiag, fill: true, tool: true },
      { key: 'scanner-settings', label: 'Settings', view: scannerSettings, tool: true },
    ],
    line: scannerLine,
    // The module is doing its work (the header's dot).
    working: s => !!s.status.live && s.status.live.state === 'live',
    // Shown on its pages while another module has the radio.
    away: 'The scanner is paused: no calls are followed until the radio is back on it.',
  },
  {
    id: 'tv',
    mode: 'atsc',
    label: 'TV',
    icon: 'tv',
    pages: [
      { key: 'watch', label: 'Watch', view: watch, fill: true },
      { key: 'channels', label: 'Channels', view: channels, fill: true },
      { key: 'tv-diag', label: 'Diagnostics', view: tvDiag, fill: true, tool: true },
    ],
    line: tvLine,
    working: () => true,
    away: 'These pages show what the last TV scan found. A scan or Play takes the radio.',
  },
  {
    id: 'data',
    mode: 'data',
    label: 'Data',
    icon: 'pulse',
    pages: [
      { key: 'scan', label: 'Scan', view: dataScan },
      { key: 'devices', label: 'Devices', view: dataDevices },
      { key: 'captures', label: 'Captures', view: dataCaptures },
    ],
    line: dataLine,
    working: s => !!s.status.data && s.status.data.state === 'listening',
    away: 'Data mode listens at 433.92 MHz for the devices that transmit nearby, lists them by frequency and reads those it knows. It listens only while it has the radio: these pages show what it last heard.',
  },
  {
    id: 'analog',
    mode: 'analog',
    label: 'Analog',
    icon: 'radio',
    pages: [
      { key: 'listen', label: 'Listen', view: analogListen },
    ],
    line: analogLine,
    working: s => !!s.status.analog && s.status.analog.state === 'listening',
    away: 'The analog receiver plays one frequency: broadcast FM, aircraft, amateur and other two-way radio, weather radio. It listens only while it has the radio; a frequency set meanwhile is where it listens next.',
  },
  {
    id: 'aircraft',
    mode: 'adsb',
    label: 'Aircraft',
    icon: 'plane',
    pages: [
      { key: 'aircraft', label: 'Aircraft', view: aircraftList },
      { key: 'map', label: 'Map', view: aircraftMap, fill: true },
    ],
    line: aircraftLine,
    working: s => !!s.status.adsb && s.status.adsb.state === 'listening',
    away: 'The Aircraft module lists the aircraft that send their positions (ADS-B) at 1090 MHz and at 978 MHz. It listens only while it has the radio: an aircraft stays listed, and on the map, for a minute after it was last heard.',
  },
  { id: 'diag', label: 'Diagnostics', title: 'Radio diagnostics', icon: 'scope', pages: [{ key: 'diag', label: 'Diagnostics', view: diag, fill: true }] },
  { id: 'files', label: 'Files', icon: 'card', pages: [{ key: 'files', label: 'Files', view: files }] },
  { id: 'settings', label: 'Settings', title: 'Radio settings', icon: 'tune', pages: [{ key: 'settings', label: 'Settings', view: settings }] },
];
