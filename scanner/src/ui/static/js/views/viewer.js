// The Viewer page (ATSC TV mode): pick a channel the last TV scan named, above the picture area.
// There is no picture yet (playback needs the transport stream in real time); the area shows what
// the scan's capture read of the channel instead: its program's streams (codec, format, bitrate,
// language), its station, and how well its signal decoded.

import { h, card } from '../dom.js';
import { mhz, num, DASH } from '../format.js';

// This browser's last channel, kept for its next visit.
const KEY = 'viewer.channel';
const remembered = () => {
  try {
    return localStorage.getItem(KEY);
  } catch {
    return null;
  }
};
const remember = v => {
  try {
    localStorage.setItem(KEY, v);
  } catch {
    // Storage refused (a private window): the pick lasts this visit only.
  }
};

// 8-VSB's transport stream rate, bit/s: what a station's programs share.
const MULTIPLEX_BPS = 19_392_658;
const mbps = bps => `${num(bps / 1e6, 2)} Mbit/s`;
const KIND = { video: 'Video', audio: 'Audio', data: 'Data' };
const hex = v => `0x${v.toString(16).toUpperCase()}`;
const utc = ms => `${new Date(ms).toISOString().replace('T', ' ').slice(0, 16)} UTC`;

// Every virtual channel the scan named, as { key, rf (the scan's row), st (its station), v }.
function channelsOf(scan) {
  const out = [];
  for (const rf of scan ? scan.found : []) {
    const st = rf.station;
    if (!st || !st.channels) continue;
    for (const v of st.channels) out.push({ key: `${rf.number}:${v.major}.${v.minor}`, rf, st, v });
  }
  return out;
}

function videoText(s) {
  const f = s.video;
  if (!f) return `${s.codec} · its format was not in the capture`;
  const scan = f.progressive == null ? '' : f.progressive ? 'p' : 'i';
  const fps = f.frame_rate ? `${num(f.frame_rate, Number.isInteger(f.frame_rate) ? 0 : 2)} fps` : null;
  return [s.codec, `${f.width}×${f.height}${scan}`, fps, f.aspect, f.profile].filter(Boolean).join(' · ');
}

function audioText(s) {
  const a = s.audio;
  if (!a) return [s.codec, s.language].filter(Boolean).join(' · ');
  return [`${s.codec} ${a.channels}`, `${num(a.sample_rate_hz / 1000, a.sample_rate_hz % 1000 ? 1 : 0)} kHz`, `${num(a.bitrate_bps / 1000)} kbit/s`, s.language]
    .filter(Boolean).join(' · ');
}

// The picture area's contents for one channel.
function screen({ rf, st, v }, scan) {
  const rows = [];
  const line = (label, text, right) => rows.push(h('span', { class: 'k', text: label }), h('span', { text }), h('span', { class: 'r', text: right || '' }));
  const prog = (st.programs || []).find(p => p.number === v.program);
  if (prog) {
    let lastKind = null;
    for (const s of prog.streams) {
      const text = s.kind === 'video' ? videoText(s) : s.kind === 'audio' ? audioText(s) : s.codec;
      line(s.kind === lastKind ? '' : KIND[s.kind] || 'Data', text, mbps(s.bitrate_bps));
      lastKind = s.kind;
    }
    if (!prog.streams.length) line('Streams', 'none in its PMT');
    line('Program', `${prog.number} · PMT ${hex(prog.pmt_pid)}${prog.pcr_pid != null ? ` · PCR ${hex(prog.pcr_pid)}` : ''}`,
      `${mbps(prog.bitrate_bps)} of ${num(MULTIPLEX_BPS / 1e6, 2)}`);
  } else {
    line('Program', `${v.program}: its PMT was not in the capture`);
  }
  const flags = [v.hidden ? 'hidden' : '', v.access_controlled ? 'scrambled' : ''].filter(Boolean).join(', ');
  line('Station', `TSID ${st.tsid ?? DASH} · ${num(st.channels.length)} channel${st.channels.length === 1 ? '' : 's'}${flags ? ` · this one ${flags}` : ''}`,
    st.null_bps ? `null ${mbps(st.null_bps)}` : '');
  line('Signal', `MER ${st.mer_db == null ? DASH : `${num(st.mer_db, 1)} dB`} · C/N ${rf.level_db == null ? DASH : `${num(rf.level_db, 1)} dB`} · `
    + `${num(st.packets)} packets, ${num(st.corrected)} corrected, ${num(st.failed)} beyond Reed-Solomon`);
  const when = scan && scan.finished_unix_ms ? `the TV scan of ${utc(scan.finished_unix_ms)}` : 'the TV scan running now';
  line('Read', `0.5 s of the channel in ${when}${st.time_unix ? `; the station's clock ${utc(st.time_unix * 1000)}` : ''}`);
  return [
    h('div', { class: 'screen-title' },
      h('strong', { text: `${v.major}.${v.minor} ${v.short_name}` }),
      v.long_name ? h('span', { class: 'dim', text: v.long_name }) : null,
      h('span', { class: 'r', text: `RF ${rf.number} · ${mhz(rf.center_hz, 0)}` })),
    h('div', { class: 'screen-stats' }, ...rows),
    h('div', { class: 'screen-note', text: 'No picture yet: playback needs the stream in real time.' }),
  ];
}

export function mount(el) {
  const c = card('Viewer');
  const pick = h('select', { class: 'input', 'aria-label': 'Channel' });
  const area = h('div', { class: 'screen' });
  c.body.append(h('div', { class: 'stack' }, h('label', { class: 'row' }, h('span', { text: 'Channel' }), pick), area));
  el.append(h('div', { class: 'stack' }, c.el));
  let last = null;
  let list = [];
  // The channels the picker holds, to rebuild it only when they change.
  let held = '';

  function show() {
    const sel = list.find(x => x.key === pick.value);
    area.replaceChildren(...(sel
      ? screen(sel, last)
      : [h('div', { class: 'screen-empty', text: list.length ? 'Pick a channel.' : 'No stations named yet: run a scan in Channels with "name the stations" on.' })]));
  }

  pick.addEventListener('change', () => {
    remember(pick.value);
    show();
  });
  show();

  return {
    update(store) {
      if (!store.atsc || store.atsc === last) return;
      last = store.atsc;
      list = channelsOf(last);
      const keys = list.map(x => x.key).join(',');
      if (keys !== held) {
        held = keys;
        const want = pick.value || remembered();
        const groups = [];
        for (const x of list) {
          if (!groups.length || groups[groups.length - 1].rf !== x.rf) groups.push({ rf: x.rf, items: [] });
          groups[groups.length - 1].items.push(x);
        }
        pick.replaceChildren(...groups.map(g => h('optgroup', { label: `RF ${g.rf.number} · ${mhz(g.rf.center_hz, 0)}${g.rf.station.tsid != null ? ` · TSID ${g.rf.station.tsid}` : ''}` },
          ...g.items.map(x => h('option', { value: x.key, text: `${x.v.major}.${x.v.minor} ${x.v.short_name}${x.v.hidden ? ' (hidden)' : ''}` })))));
        pick.value = list.some(x => x.key === want) ? want : list.length ? list[0].key : '';
        pick.disabled = !list.length;
      }
      show();
    },
    unmount() {},
  };
}
