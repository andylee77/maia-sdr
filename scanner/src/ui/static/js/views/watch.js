// Watch (the TV module): the channels the last TV scan named, the picture with its controls and
// the programme's streams, and the signal. A channel plays live when the radio core has the
// 8-VSB block: the radio is given to TV first when another module has it, and the unit tunes the
// channel before the player asks for its stream. Without the block a recording of its RF channel
// on the card plays. Live TV's state comes from the radio as it changes. The stream plays in VLC
// or another player too, from its address.
//
// Three views, cycled by the View button or a double click on the picture: small (the channels
// down the left, the picture in the middle, the signal on the right), large (the picture across
// the page, the channels a strip below it) and full screen (the picture alone, nothing over it).

import { h, toast, setText, setClass, keyedList, icon, iconButton } from '../dom.js';
import { mhz, num, DASH } from '../format.js';
import { api } from '../api.js';
import { want } from '../store.js';
import { giveRadio } from '../mode.js';
import { tv, streamUrl } from '../tv/player.js';
import { channelsOf, pickedChannel, pickChannel, pickedView, pickView } from '../tv/pick.js';

// 8-VSB's transport stream rate, bit/s: what a station's programmes share.
const MULTIPLEX_BPS = 19_392_658;
// The MER the bars run from and to, dB (8-VSB decodes from about 15).
const MER_LOW = 10;
const MER_HIGH = 35;
const KIND = { video: 'Video', audio: 'Audio', data: 'Data' };
const LIVE_STATE = { starting: 'Starting', searching: 'Finding the signal', playing: 'Playing', error: 'Failed' };

const mbps = bps => `${num(bps / 1e6, 2)} Mbit/s`;
const hex = v => `0x${v.toString(16).toUpperCase()}`;
const utc = ms => `${new Date(ms).toISOString().replace('T', ' ').slice(0, 16)} UTC`;
const known = v => v !== null && v !== undefined;
const dB = v => (known(v) ? `${num(v, 1)} dB` : DASH);
// A signed value at `digits` decimals; one that rounds to nothing is 0, not -0.
const signed = (v, digits = 0) => num(Math.abs(v) < 0.5 / 10 ** digits ? 0 : v, digits);
const chName = v => `${v.major}.${v.minor} ${v.short_name}`;

function videoText(s) {
  const f = s.video;
  if (!f) return `${s.codec} · its format was not in the capture`;
  const scan = !known(f.progressive) ? '' : f.progressive ? 'p' : 'i';
  const fps = f.frame_rate ? `${num(f.frame_rate, Number.isInteger(f.frame_rate) ? 0 : 2)} fps` : null;
  return [s.codec, `${f.width}×${f.height}${scan}`, fps, f.aspect, f.profile].filter(Boolean).join(' · ');
}

function audioText(s) {
  const a = s.audio;
  if (!a) return [s.codec, s.language].filter(Boolean).join(' · ');
  return [`${s.codec} ${a.channels}`, `${num(a.sample_rate_hz / 1000, a.sample_rate_hz % 1000 ? 1 : 0)} kHz`, `${num(a.bitrate_bps / 1000)} kbit/s`, s.language]
    .filter(Boolean).join(' · ');
}

// A playlist naming the stream, for VLC and other players to open.
function playlist(rf, v) {
  const body = `#EXTM3U\n#EXTINF:-1,${chName(v)}\n${streamUrl(rf, v.program)}\n`;
  const a = h('a', { href: URL.createObjectURL(new Blob([body], { type: 'audio/x-mpegurl' })), download: `rf${rf}-${v.program}.m3u` });
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
}

// What the last TV scan's capture read of a channel's programme: [label, text, right] rows.
function programRows({ rf, st, v }, scan) {
  const rows = [];
  const prog = (st.programs || []).find(p => p.number === v.program);
  if (prog) {
    let lastKind = null;
    for (const s of prog.streams) {
      const text = !s.bitrate_bps ? `${s.codec} · nothing sent in the capture`
        : s.kind === 'video' ? videoText(s) : s.kind === 'audio' ? audioText(s) : s.codec;
      rows.push([s.kind === lastKind ? '' : KIND[s.kind] || 'Data', text, mbps(s.bitrate_bps)]);
      lastKind = s.kind;
    }
    if (!prog.streams.length) rows.push(['Streams', 'none in its PMT', '']);
    rows.push(['Program', `${prog.number} · PMT ${hex(prog.pmt_pid)}${known(prog.pcr_pid) ? ` · PCR ${hex(prog.pcr_pid)}` : ''}`,
      `${mbps(prog.bitrate_bps)} of ${num(MULTIPLEX_BPS / 1e6, 2)}`]);
  } else {
    rows.push(['Program', `${v.program}: its PMT was not in the capture`, '']);
  }
  const flags = [v.hidden ? 'hidden' : '', v.access_controlled ? 'scrambled' : ''].filter(Boolean).join(', ');
  rows.push(['Station', `TSID ${st.tsid ?? DASH} · ${num(st.channels.length)} channel${st.channels.length === 1 ? '' : 's'}${flags ? ` · this one ${flags}` : ''}`,
    st.null_bps ? `null ${mbps(st.null_bps)}` : '']);
  const when = scan && scan.finished_unix_ms ? `the TV scan of ${utc(scan.finished_unix_ms)}` : 'the TV scan running now';
  rows.push(['Read', `in ${when}${st.time_unix ? `; the station's clock ${utc(st.time_unix * 1000)}` : ''}`, '']);
  return rows;
}

// The signal while a channel plays live: the block's loops and its counts.
function liveRows(s) {
  const rows = [
    ['Receiver', LIVE_STATE[s.state] || s.state],
    ['Carrier offset', known(s.carrier_hz) ? `${signed(s.carrier_hz)} Hz` : DASH],
    ['Timing', known(s.timing_ppm) ? `${signed(s.timing_ppm, 1)} ppm` : DASH],
    ['Codewords', num(s.codewords)],
    ['Corrected', num(s.corrected)],
    ['Beyond Reed-Solomon', num(s.failed)],
    ['Equalizer solves', known(s.solve_ms) ? `${num(s.solves)} · ${num(s.solve_ms)} ms` : num(s.solves)],
  ];
  if (s.missed || s.dropped) rows.push(['Packets lost', `${num(s.missed)} missed, ${num(s.dropped)} dropped`]);
  if (s.restarts) rows.push(['Restarts', num(s.restarts)]);
  rows.push(['On the stream', `${num(s.viewers)} player${s.viewers === 1 ? '' : 's'}`]);
  return rows;
}

// The signal as the last TV scan read it.
function scanRows({ rf, st }) {
  return [
    ['Carrier to noise', dB(rf.level_db)],
    ['Pilot', known(rf.pilot_db) ? `${num(rf.pilot_db, 1)} dB over the plateau` : DASH],
    ['Power', known(rf.power_dbm) ? `${num(rf.power_dbm, 1)} dBm` : DASH],
    ['Packets', num(st.packets)],
    ['Corrected', num(st.corrected)],
    ['Beyond Reed-Solomon', num(st.failed)],
  ];
}

function createChannelRow(pick) {
  const c = {
    n: h('span', { class: 'tv-num' }),
    name: h('span', { class: 'tv-name' }),
    sub: h('span', { class: 'sub' }),
    sig: h('span', { class: 'tv-sig' }),
  };
  const row = h('button', { class: 'tv-row', type: 'button' }, c.n, h('span', { class: 'tv-text' }, c.name, c.sub), c.sig);
  row._c = c;
  row.addEventListener('click', () => pick(row._x.key));
  return row;
}

export function mount(el) {
  // The channel list.
  const count = h('span', { class: 'dim' });
  const find = h('input', { class: 'input', type: 'search', placeholder: 'Find', 'aria-label': 'Find a channel' });
  const rows = h('div', { class: 'tv-list' });
  const none = h('div', { class: 'calls-empty' });
  const listCard = h('section', { class: 'card flush tv-channels', 'aria-label': 'Channels' },
    h('div', { class: 'card-head' }, h('h2', { text: 'Channels' }), count, h('div', { class: 'spacer' }), find),
    h('div', { class: 'card-body' }, rows, none));

  // The picture, its controls and the programme.
  const canvas = h('canvas', { class: 'tv-picture' });
  const overTitle = h('div', { class: 'tv-over-title' });
  const overNote = h('div', { class: 'tv-over-note' });
  const over = h('div', { class: 'tv-over' }, overTitle, overNote);
  const screen = h('div', { class: 'tv-screen' }, canvas, over);
  const playBtn = h('button', { class: 'tv-play', type: 'button' });
  const prev = iconButton('down', 'The channel before', { cls: 'tv-step' });
  const next = iconButton('up', 'The next channel', { cls: 'tv-step' });
  const title = h('div', { class: 'tv-title' });
  const state = h('div', { class: 'sub' });
  const vlc = h('a', { class: 'btn small', text: 'Open in VLC', title: 'A playlist naming this channel\'s stream, for VLC or another player', href: '#' });
  const viewText = h('span');
  const viewBtn = h('button', { class: 'btn small tv-view', type: 'button' }, icon('fullscreen'), viewText);
  const other = h('div', { class: 'tv-other', hidden: true });
  const bar = h('div', { class: 'card tv-bar' }, playBtn, prev, next, h('div', { class: 'tv-now' }, title, state), h('div', { class: 'spacer' }), vlc, viewBtn);
  const program = h('div', { class: 'card tv-program' });
  const stage = h('div', { class: 'tv-stage' }, screen, bar, other, program);

  // The signal.
  const sigTitle = h('h2', { text: 'Signal' });
  const sigKicker = h('div', { class: 'kicker' });
  const merValue = h('span', { class: 'tv-mer' });
  const merBars = h('div', { class: 'tv-bars', 'aria-hidden': 'true' });
  const sigRows = h('dl', { class: 'kv' });
  const sigNote = h('p', { class: 'card-note' });
  const signal = h('section', { class: 'card tv-signal', 'aria-label': 'Signal' },
    h('div', { class: 'card-head' }, sigTitle), sigKicker,
    h('div', { class: 'tv-mer-line' }, merValue, h('span', { class: 'dim', text: 'dB MER' })), merBars, sigRows, sigNote);

  const root = h('div', { class: 'watch' }, listCard, stage, signal);
  el.append(root);

  // The view in the page (small or large); full screen is the browser's, over either.
  let view = pickedView();
  function showView() {
    setClass(root, 'large', view === 'large');
    setText(viewText, view === 'large' ? 'Large view' : 'Small view');
    viewBtn.title = view === 'large' ? 'Large view: next, full screen (the picture alone)' : 'Small view: next, the large view (the picture across the page)';
  }
  // Small, large, full screen, and small again. Out of full screen by Esc, the view is as it was.
  function cycleView() {
    if (document.fullscreenElement) {
      document.exitFullscreen().catch(() => {});
      view = 'small';
    } else if (view === 'small') {
      view = 'large';
    } else {
      screen.requestFullscreen().catch(() => toast('This browser refused full screen', true));
      return;
    }
    pickView(view);
    showView();
    draw();
  }
  viewBtn.addEventListener('click', cycleView);
  screen.addEventListener('dblclick', cycleView);
  showView();

  // The last TV scan and its channels; the picked channel's key.
  let scan = null;
  let list = [];
  let picked = pickedChannel();
  // RF channel -> the programmes its recording on the card holds.
  let recorded = new Map();
  // Live TV's state as the radio last pushed it.
  let live = null;
  let player = tv.status();
  // A start is under way (the radio being given, the channel being tuned), and another channel
  // was picked meanwhile.
  let starting = false;
  let again = false;
  // What the play button last showed, and the pick last brought into view.
  let shownOn = null;
  let shownPick = null;
  let drawnProgram = null;
  let drawnSignal = null;

  const current = () => list.find(x => x.key === picked) || null;
  const mine = x => !!x && !!player.playing && player.playing.rf === x.rf.number && player.playing.program === x.v.program;
  const onAir = () => !!(live && live.available);
  // The unit's receiver is on RF channel `rf` (starting, finding the signal, or playing).
  const receiving = rf => !!live && live.rf === rf && ['starting', 'searching', 'playing'].includes(live.state);
  const hasRecording = x => (recorded.get(x.rf.number) || []).includes(x.v.program);

  // Play the picked channel: live when the radio core can, else its recording. A channel picked
  // while one is starting takes its place.
  async function play() {
    if (starting) {
      again = true;
      return;
    }
    starting = true;
    draw();
    try {
      for (;;) {
        again = false;
        const x = current();
        if (!x) break;
        if (!onAir() && !hasRecording(x)) {
          toast(live ? `This radio core has no 8-VSB block, and the card has no recording of RF ${x.rf.number} (tv/rf${x.rf.number}.ts)` : 'The radio has not said yet whether it can play live TV', true);
          break;
        }
        if (onAir()) {
          if (!(await giveRadio('atsc', 'TV'))) break;
          // Tuned before the player asks for the stream, so it never meets the last channel's.
          // A channel of the RF channel the receiver is on plays from the stream as it runs.
          if (!receiving(x.rf.number)) await api.atscLiveStart(x.rf.number);
        }
        if (again) continue;
        tv.start(x.rf.number, x.v.program);
        break;
      }
    } catch (e) {
      toast(e.message, true);
    } finally {
      starting = false;
      draw();
    }
  }

  // The unit's receiver stops with this page's player unless another player is on its stream.
  function stopLive() {
    if (onAir() && live.state !== 'idle' && (live.viewers || 0) <= 1) api.atscLiveStop().catch(e => toast(e.message, true));
  }

  function stop() {
    const was = !!player.playing;
    tv.stop();
    if (was) stopLive();
  }

  // Pick a channel; while one plays, the new one plays in its place.
  function pick(key) {
    if (key === picked) return;
    const playing = !!player.playing || starting;
    picked = key;
    pickChannel(key);
    if (playing) {
      // The sound belongs to the channel it was started on.
      tv.stop();
      play();
    }
    draw();
  }

  function step(by) {
    if (!list.length) return;
    const i = list.findIndex(x => x.key === picked);
    pick(list[(Math.max(i, 0) + by + list.length) % list.length].key);
  }

  playBtn.addEventListener('click', () => (player.playing ? stop() : play()));
  prev.addEventListener('click', () => step(-1));
  next.addEventListener('click', () => step(1));
  vlc.addEventListener('click', e => {
    e.preventDefault();
    const x = current();
    if (x) playlist(x.rf.number, x.v);
  });
  find.addEventListener('input', () => draw());
  // A browser starts sound only after a click on the page.
  el.addEventListener('click', () => tv.resume());

  function drawList(x) {
    const q = find.value.trim().toLowerCase();
    const shown = q ? list.filter(c => chName(c.v).toLowerCase().includes(q) || (c.v.long_name || '').toLowerCase().includes(q) || `rf ${c.rf.number}`.includes(q)) : list;
    keyedList(rows, shown, c => c.key, () => createChannelRow(pick), (row, c) => {
      row._x = c;
      setText(row._c.n, `${c.v.major}.${c.v.minor}`);
      setText(row._c.name, c.v.short_name);
      setText(row._c.sub, [`RF ${c.rf.number}`, c.v.long_name, c.v.hidden ? 'hidden' : '', c.v.access_controlled ? 'scrambled' : ''].filter(Boolean).join(' · '));
      setText(row._c.sig, known(c.st.mer_db) ? `${num(c.st.mer_db, 0)} dB` : '');
      row._c.sig.title = 'MER in the last TV scan';
      setClass(row, 'sel', !!x && c.key === x.key);
      setClass(row, 'live', mine(c));
      if (x && c.key === x.key) row.setAttribute('aria-current', 'true');
      else row.removeAttribute('aria-current');
    });
    // A newly picked channel is brought into the list's view (or the strip's).
    const key = x ? `${x.key}|${view}` : '';
    if (key !== shownPick) {
      shownPick = key;
      const row = rows.querySelector('.tv-row.sel');
      if (row) row.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    }
    setText(count, list.length ? num(list.length) : '');
    none.hidden = shown.length > 0;
    setText(none, list.length ? 'No channel matches.' : '');
  }

  function drawStage(x) {
    const playing = mine(x);
    setClass(screen, 'playing', playing && player.picture);
    over.hidden = playing && player.picture;
    if (!x) {
      setText(overTitle, scan ? 'No channels yet' : 'Reading the TV scan…');
      overNote.replaceChildren(...(scan ? [
        h('span', { text: 'A TV scan names the stations on the air. It takes about two minutes and needs the radio.' }),
        h('a', { class: 'btn primary', href: '#channels', text: 'Go to Channels' }),
      ] : []));
    } else {
      setText(overTitle, chName(x.v));
      // Starting, the player's state, why it stopped, or what Play would do.
      setText(overNote, starting ? 'Starting…'
        : playing ? player.label || 'Starting…'
          : player.label ? player.label
            : onAir() || hasRecording(x) ? 'Press play to watch.'
              : live ? `No live TV on this radio core, and no recording of RF ${x.rf.number} on the card.` : '');
    }
    const on = !!player.playing;
    if (on !== shownOn) {
      shownOn = on;
      playBtn.replaceChildren(icon(on ? 'stop' : 'play', 20));
      playBtn.title = on ? 'Stop' : 'Play';
      playBtn.setAttribute('aria-label', playBtn.title);
    }
    playBtn.disabled = !x || starting;
    prev.disabled = next.disabled = list.length < 2;
    setText(title, x ? chName(x.v) : 'TV');
    setText(state, !x ? '' : [
      `RF ${x.rf.number} · ${mhz(x.rf.center_hz, 0)}`,
      playing ? (onAir() ? 'live' : 'its recording, in a loop') : '',
      playing ? player.label : '',
    ].filter(Boolean).join(' · '));
    state.title = playing ? player.detail : '';
    vlc.hidden = !x;
    // Live TV running for another player (VLC, another browser): it holds the radio until stopped.
    const elsewhere = !!live && live.state !== 'idle' && live.state !== 'error' && !player.playing && !starting;
    other.hidden = !elsewhere;
    if (elsewhere && other.dataset.k !== String(live.rf)) {
      other.dataset.k = String(live.rf);
      other.replaceChildren(h('span', { text: `Live TV is on RF ${live.rf} for another player, and holds the radio until it is stopped.` }),
        h('button', { class: 'btn small', type: 'button', text: 'Stop live TV', onclick: () => api.atscLiveStop().catch(e => toast(e.message, true)) }));
    }
  }

  function drawProgram(x) {
    const key = x ? `${x.key}|${scan.id}|${scan.finished_unix_ms}` : '';
    if (key === drawnProgram) return;
    drawnProgram = key;
    program.hidden = !x;
    if (!x) return;
    const url = streamUrl(x.rf.number, x.v.program);
    program.replaceChildren(
      ...programRows(x, scan).flatMap(([k, text, right]) => [h('span', { class: 'k', text: k }), h('span', { text }), h('span', { class: 'r', text: right })]),
      // The stream's address, for another player.
      h('span', { class: 'k', text: 'Stream' }), h('a', { href: url, text: url }), h('span', { class: 'r' }));
  }

  function drawSignal(x) {
    // Live TV's own numbers while this channel's RF plays; else what the scan read of it.
    const now = !!x && receiving(x.rf.number);
    const mer = now ? live.mer_db : [];
    const key = JSON.stringify([x && x.key, now, now ? live : null, live && live.error, live && live.available]);
    if (key === drawnSignal) return;
    drawnSignal = key;
    signal.hidden = !x;
    if (!x) return;
    setText(sigKicker, now ? 'Live, as the receiver reads it' : 'As the last TV scan read it');
    setText(merValue, now ? (mer.length ? num(mer[mer.length - 1], 1) : DASH) : known(x.st.mer_db) ? num(x.st.mer_db, 1) : DASH);
    merBars.hidden = !mer.length;
    merBars.replaceChildren(...mer.map(v => h('span', { style: { height: `${Math.round(Math.max(0.06, Math.min(1, (v - MER_LOW) / (MER_HIGH - MER_LOW))) * 100)}%` } })));
    merBars.title = `The last ${mer.length} fields' MER, ${MER_LOW} to ${MER_HIGH} dB`;
    sigRows.replaceChildren(...(now ? liveRows(live) : scanRows(x)).flatMap(([k, v]) => [h('dt', { text: k }), h('dd', { text: v })]));
    setText(sigNote, live && live.state === 'error' ? `Live TV failed: ${live.error || 'no reason given'}`
      : live && !live.available ? 'This radio core has no 8-VSB block: a channel plays from its recording on the card, if it has one.' : '');
    sigNote.hidden = !sigNote.textContent;
  }

  function draw() {
    if (picked && list.length && !list.some(x => x.key === picked)) picked = null;
    if (!picked && list.length) picked = list[0].key;
    const x = current();
    drawList(x);
    drawStage(x);
    drawProgram(x);
    drawSignal(x);
  }

  tv.attach(canvas);
  const offPlayer = tv.onChange(s => {
    player = s;
    draw();
  });
  const offLive = want({ atsc_live: true }, m => {
    live = m.live;
    draw();
  });
  api.atscRecordings()
    .then(rs => {
      recorded = new Map(rs.map(r => [r.rf, r.programs]));
      draw();
    })
    .catch(() => { /* none listed: nothing recorded to play */ });
  draw();

  return {
    update(store) {
      if (!store.atsc || store.atsc === scan) return;
      scan = store.atsc;
      list = channelsOf(scan);
      drawnProgram = null;
      draw();
    },
    unmount() {
      offPlayer();
      const was = !!player.playing;
      tv.stop();
      tv.attach(null);
      if (was) stopLive();
      offLive();
    },
  };
}
