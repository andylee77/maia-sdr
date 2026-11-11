// The Scanner's settings: the gain it receives with, the window presets its planner may use,
// when a call closes, how long the history is kept, what is recorded and where, and packet
// data. Each card saves its own part. The radio's own settings (the clock, the crystal, the
// configuration as a file) are the rail's Settings.

import { h, toast, setClass, switchInput } from '../dom.js';
import { api } from '../api.js';
import { bytes, num } from '../format.js';
import { packetDataSetting } from '../protocols.js';
import { RECORD, recordKey } from '../recording.js';
import { row, unit, count, typed, foot, section, settingsPage } from './settings_parts.js';

const MODES = [['slow_attack', 'AGC, slow'], ['fast_attack', 'AGC, fast'], ['hybrid', 'AGC, hybrid'], ['manual', 'Manual']];

function gainCard() {
  const sec = section('Gain');
  const mode = h('select', { class: 'input', 'aria-label': 'Gain mode' }, ...MODES.map(([v, t]) => h('option', { value: v, text: t })));
  const db = h('input', { class: 'input num', type: 'number', min: -3, max: 76, step: 1, 'aria-label': 'Manual gain, dB' });
  const apply = h('button', { class: 'btn', type: 'button', text: 'Apply' });
  const sync = () => { db.disabled = mode.value !== 'manual'; };
  mode.addEventListener('change', sync);
  sec.edits(mode, db);
  apply.addEventListener('click', async () => {
    try {
      await api.setGain(mode.value, mode.value === 'manual' ? typed(db, 'Manual gain', { whole: false }) : null);
      sec.saved();
      toast('Gain applied');
    } catch (e) {
      toast(e.message, true);
    }
  });
  sec.body.append(row('The receiver\'s gain', [mode, db, unit('dB'), apply], 'Applied at once. TV sets its own gain while it has the radio.'));
  return {
    el: sec.el,
    show({ radio }) {
      if (sec.touched) return;
      mode.value = radio.config.gain.mode;
      db.value = radio.config.gain.manual_db ?? 60;
      sync();
    },
  };
}

function windowCard({ radio }) {
  const sec = section('Receive window');
  const presets = radio.presets.map(name => {
    const box = h('input', { type: 'checkbox', value: name });
    return { name, box, el: h('label', { class: 'chip' }, box, h('span', { text: name })) };
  });
  sec.edits(...presets.map(p => p.box));
  const save = h('button', { class: 'btn primary', type: 'button', text: 'Save' });
  save.addEventListener('click', async () => {
    try {
      await api.saveRadioSettings({ presets_allowed: presets.filter(p => p.box.checked).map(p => p.name) });
      sec.saved();
      toast('Saved; it applies at the next site switch');
    } catch (e) {
      toast(e.message, true);
    }
  });
  sec.body.append(
    h('div', { class: 'set-row wide' },
      h('div', { class: 'set-what' }, h('div', { text: 'Window presets the planner may use' })),
      h('div', { class: 'chips' }, ...presets.map(p => p.el))),
    foot('The radio receives one slice of spectrum at a time, and the planner picks its width from these. It applies at the next site switch.', save));
  return {
    el: sec.el,
    show(d) {
      if (sec.touched) return;
      for (const p of presets) p.box.checked = d.radio.config.presets_allowed.includes(p.name);
    },
  };
}

function callsCard() {
  const sec = section('Calls');
  const hang = count(500, 30000, 'Hang time, ms');
  const grace = count(0, 10000, 'End grace, ms');
  sec.edits(hang, grace);
  const save = h('button', { class: 'btn primary', type: 'button', text: 'Save' });
  save.addEventListener('click', async () => {
    try {
      await api.saveRadioSettings({ calls: { hang_ms: typed(hang, 'A call closes after'), end_grace_ms: typed(grace, 'A call closes after its end') } });
      sec.saved();
      toast('Saved; it applies at the next site switch');
    } catch (e) {
      toast(e.message, true);
    }
  });
  sec.body.append(
    row('A call closes after', [hang, unit('ms')], 'With no sign of life.'),
    row('…or after its end', [grace, unit('ms')]),
    foot('The call timings apply at the next site switch.', save));
  return {
    el: sec.el,
    show({ radio }) {
      if (sec.touched) return;
      hang.value = radio.config.calls.hang_ms;
      grace.value = radio.config.calls.end_grace_ms;
    },
  };
}

function historyCard() {
  const sec = section('History');
  const days = count(1, 3650, 'History, days');
  const mb = count(16, 32768, 'History, MB');
  sec.edits(days, mb);
  // What the radio holds, to tell a lowered limit by.
  let held = null;
  const save = h('button', { class: 'btn primary', type: 'button', text: 'Save' });
  save.addEventListener('click', async () => {
    try {
      const history = { retention_days: typed(days, 'The history is kept'), sd_max_mb: typed(mb, 'The history holds at most') };
      // A lowered limit drops the calls past it as it is saved.
      const lowered = held && (history.retention_days < held.retention_days || history.sd_max_mb < held.sd_max_mb);
      if (lowered && !confirm('A lower limit deletes the history past it at once. This cannot be undone. Save?')) return;
      await api.saveRadioSettings({ history });
      sec.saved();
      toast('Saved');
    } catch (e) {
      toast(e.message, true);
    }
  });
  sec.body.append(
    row('The history is kept', [days, unit('days')]),
    row('The history holds at most', [mb, unit('MB')]),
    foot('Every call the scanner has seen, on the card. Applied at once.', save));
  return {
    el: sec.el,
    show({ radio }) {
      held = radio.config.history;
      if (sec.touched) return;
      days.value = held.retention_days;
      mb.value = held.sd_max_mb;
    },
  };
}

// `reload()` reads the page's settings again (after recordings are deleted: their counts).
function recordingCard(reload) {
  const sec = section('Recording');
  // The recording settings the radio holds, to tell a lowered limit by.
  let held = null;
  const rec = RECORD.map(([key, text, title]) => h('button', { type: 'button', text, title, dataset: { key } }));
  for (const b of rec) {
    b.addEventListener('click', async () => {
      try {
        await api.setRecording(RECORD.find(x => x[0] === b.dataset.key)[3]);
        toast(b.dataset.key === 'marked' ? 'Recording the talkgroups marked Rec' : b.dataset.key === 'every' ? 'Recording every followed call' : 'Recording is off');
      } catch (e) {
        toast(e.message, true);
      }
    });
  }
  const where = h('select', { class: 'input', 'aria-label': 'New recordings go to' }, h('option', { value: 'sd', text: 'SD card' }), h('option', { value: 'ram', text: 'RAM (lost on reboot)' }));
  const ramN = count(1, 10000, 'RAM keeps, recordings');
  const sdDays = count(1, 3650, 'SD card keeps, days');
  const lockedDays = count(1, 3650, 'Locked ones, days');
  const sdMb = count(16, 1048576, 'SD card, at most MB');
  const perFolder = count(100, 6000, 'Recordings a folder');
  sec.edits(where, ramN, sdDays, lockedDays, sdMb, perFolder);
  const save = h('button', { class: 'btn primary', type: 'button', text: 'Save' });
  save.addEventListener('click', async () => {
    try {
      const next = {
        storage: where.value,
        ram_max_count: typed(ramN, 'RAM keeps'), sd_keep_days: typed(sdDays, 'The card keeps a recording'), locked_keep_days: typed(lockedDays, 'A locked recording'),
        sd_max_mb: typed(sdMb, 'The card holds at most'), folder_max_count: typed(perFolder, "A day's folder holds at most"),
      };
      // A lowered limit deletes what is past it as it is saved.
      const lowered = held && ['ram_max_count', 'sd_keep_days', 'locked_keep_days', 'sd_max_mb'].some(k => next[k] < held[k]);
      if (lowered && !confirm('A lower limit deletes the recordings past it at once. This cannot be undone. Save?')) return;
      const res = await api.setRecording(next);
      sec.saved();
      toast(res.deleted ? `Saved; ${res.deleted} oldest recording(s) deleted` : 'Saved');
    } catch (e) {
      toast(e.message, true);
    }
  });
  const clearBtn = (label, which, what) => h('button', {
    class: 'btn small danger', type: 'button', text: label,
    onclick: async () => {
      if (!confirm(`Delete ${what}? This cannot be undone.`)) return;
      try {
        const res = await api.clearRecordings(which);
        toast(`${res.deleted} recording(s) deleted`);
        reload();
      } catch (e) {
        toast(e.message, true);
      }
    },
  });
  const state = h('p', { class: 'card-note' });
  sec.body.append(
    row('What is recorded', h('div', { class: 'seg', role: 'group', 'aria-label': 'What is recorded' }, ...rec), 'Off, the talkgroups marked Rec (on Now and Aliases), or every followed call. Saved as it is picked; Now has the same choice.'),
    row('New recordings go to', where),
    row('RAM keeps', [ramN, unit('recordings')]),
    row('The card keeps a recording', [sdDays, unit('days')]),
    row('A locked recording', [lockedDays, unit('days')], 'Calls with alert tones are locked; any recording on the card can be locked from Now or Activity.'),
    row('The card holds at most', [sdMb, unit('MB')], 'Past it, the oldest unlocked go first.'),
    row('A day\'s folder holds at most', [perFolder, unit('recordings')]),
    foot(state, save),
    foot('', clearBtn('Delete SD recordings', 'sd', 'every recording on the SD card'), clearBtn('Delete RAM recordings', 'ram', 'every recording in RAM')));
  return {
    el: sec.el,
    show({ radio, recs }) {
      held = radio.config.recording;
      const key = recordKey(held);
      for (const b of rec) {
        setClass(b, 'on', b.dataset.key === key);
        b.setAttribute('aria-pressed', String(b.dataset.key === key));
      }
      const st = recs.storage;
      const sd = st.sd_status;
      const free = sd.free_bytes !== null && sd.free_bytes !== undefined ? `, ${bytes(sd.free_bytes)} free` : '';
      state.textContent = `SD card (${sd.state.replace('_', ' ')}${free}): ${num(st.sd.count)} recordings, ${bytes(st.sd.bytes)}, ${num(st.locked.count)} locked. RAM: ${num(st.ram.count)}, ${bytes(st.ram.bytes)}.${sd.state === 'failing' && sd.detail ? ` The last write failed: ${sd.detail}` : ''}`;
      if (sec.touched) return;
      where.value = held.storage;
      ramN.value = held.ram_max_count;
      sdDays.value = held.sd_keep_days;
      lockedDays.value = held.locked_keep_days;
      sdMb.value = held.sd_max_mb;
      perFolder.value = held.folder_max_count;
    },
  };
}

// Packet data followed or not; the radio takes a change at once.
function packetDataCard() {
  const sec = section('Packet data');
  const follow = switchInput('', false, async on => {
    try {
      const kept = await api.setPacketData(on);
      toast(kept.follow ? 'Data traffic is followed' : 'Data traffic is not followed');
    } catch (e) {
      follow.input.checked = !on;
      toast(e.message, true);
    }
  });
  follow.input.setAttribute('aria-label', packetDataSetting.label);
  sec.body.append(row(packetDataSetting.label, follow.el, packetDataSetting.note));
  return {
    el: sec.el,
    show({ radio }) {
      follow.input.checked = radio.config.packet_data.follow;
    },
  };
}

export function mount(el) {
  return settingsPage(el, {
    read: async () => {
      const [radio, recs] = await Promise.all([api.radio(), api.recordings(0)]);
      return { radio, recs };
    },
    build: (data, reload) => [
      [gainCard(), windowCard(data), callsCard(), historyCard()],
      [recordingCard(reload), packetDataCard()],
    ],
    link: ['The radio\'s own settings (the clock, the crystal, the configuration as a file) are under ', h('a', { href: '#settings', text: 'Settings' }), ' in the rail.'],
  });
}
