// Recent calls with inline playback (Now view).
//
// Rows come from /api/ui/calls: one per grant (grant = call), joined
// with its recording by call_id. Rows are keyed by call_id so a playing
// <audio> survives every refresh. Voice length is the WAV length (or
// IMBE frames x 20 ms), never the lifecycle's open time, which includes
// the time after the last voice until the close.

import { h, setText, keyedList, card } from '../dom.js';
import { mhz, dur, ago, clock, bytes, linkControlUnits, NOT_FOLLOWED, AUDIO_STATUS, CLOSE_REASON } from '../format.js';
import { store, boardNow, setPref } from '../store.js';

function when(call) {
  const s = store.state;
  const wall = clock(call.started_unix_ms, s && s.clock_valid);
  return wall || ago(Math.max(0, boardNow() - call.started_unix_ms));
}

// Change 065: three different times, labelled for what they measure.
// Voice = the decoded audio (the recording / playback bar); on air = the
// control channel announcing the call (grant to its last update, the
// channel time SDRTrunk shows, also for encrypted calls); held = how long
// the follower kept the call open (it waits after the end).
function detailRows(c) {
  const reason = CLOSE_REASON[c.close_reason] || c.close_reason.replace(/_/g, ' ');
  const rows = [
    ['Call id', String(c.call_id)],
    ['Voice', c.voice_ms ? dur(c.voice_ms) + (c.recording ? ' (the recording)' : '') : '—'],
    ['On air (control channel)', c.air_ms != null ? dur(c.air_ms) : '—'],
    [c.not_followed ? 'Channel time' : 'Held open', dur(c.open_ms) + (c.not_followed ? '' : ' (' + reason + ')')],
    ['First voice', c.first_voice_ms != null ? c.first_voice_ms + ' ms after grant' : '—'],
    ['IMBE / LDU', c.imbe + ' / ' + c.ldu],
    ['Vocoder errors / silent', c.vocoder_errors + ' / ' + c.vocoder_silent],
  ];
  const lc = linkControlUnits(c);
  if (lc.length) rows.push(['Link control ID', lc.join(', ') + ' (differs from the grant)']);
  if (c.recording) {
    const where = c.recording.storage === 'sd' ? 'SD card' : 'RAM';
    rows.push(['File', c.recording.filename + ' (' + bytes(c.recording.size_bytes) + ', ' + where + ')']);
  }
  return rows;
}

function createRow() {
  const el = h('div', { class: 'call-row' });
  el._c = {
    when: h('div', { class: 'when c-when' }),
    tg: h('div', { class: 'tg' }), tgSub: h('div', { class: 'sub' }),
    src: h('div'), srcSub: h('div', { class: 'sub' }),
    freq: h('div', { class: 'num' }), freqSub: h('div', { class: 'sub' }),
    dur: h('div', { class: 'dur' }), durSub: h('div', { class: 'sub dur' }),
    play: h('div', { class: 'play c-play' }),
    details: h('dl', { class: 'kv', hidden: true, style: { gridColumn: '1 / -1', margin: '6px 0 2px' } }),
  };
  const c = el._c;
  el.append(
    c.when,
    h('div', { class: 'c-tg' }, c.tg, c.tgSub),
    h('div', { class: 'c-src' }, c.src, c.srcSub),
    h('div', { class: 'c-freq' }, c.freq, c.freqSub),
    h('div', { class: 'c-dur' }, c.dur, c.durSub),
    c.play,
    c.details,
  );
  return el;
}

function renderPlay(el, call) {
  const c = el._c;
  const sig = (call.recording ? call.recording.url : '') + '|' + call.audio_status;
  if (c.play.dataset.sig === sig) return;
  c.play.dataset.sig = sig;
  c.play.replaceChildren();
  const info = h('button', {
    class: 'btn small icon', title: 'Details', 'aria-label': 'Details',
    onclick: () => { c.details.hidden = !c.details.hidden; },
  }, 'i');
  if (call.recording) {
    const url = call.recording.url;
    const play = h('button', { class: 'btn small', title: 'Play recording' }, '▶ ' + dur(call.recording.duration_ms));
    play.addEventListener('click', () => {
      const audio = h('audio', { controls: true, preload: 'auto', src: url });
      play.replaceWith(audio);
      audio.play().catch(() => { /* needs another tap on some phones */ });
    });
    c.play.append(play, h('a', { href: url, download: call.recording.filename, class: 'btn small icon', title: 'Download WAV' }, '⤓'), info);
  } else {
    c.play.append(h('span', { class: 'why', text: AUDIO_STATUS[call.audio_status] || call.audio_status }), info);
  }
}

function updateRow(el, call) {
  const c = el._c;
  el.classList.toggle('nf', !!call.not_followed);
  setText(c.when, when(call));
  setText(c.tg, call.tg_alias || 'TG ' + call.tg);
  const tgBits = [];
  if (call.tg_alias) tgBits.push('TG ' + call.tg);
  if (call.encrypted) tgBits.push('encrypted');
  else if (call.not_followed) tgBits.push(NOT_FOLLOWED[call.not_followed] || call.not_followed);
  setText(c.tgSub, tgBits.join(' · '));
  setText(c.src, call.source_alias || (call.source ? String(call.source) : 'unknown'));
  setText(c.srcSub, call.source_alias ? String(call.source) : '');
  setText(c.freq, mhz(call.freq_hz));
  setText(c.freqSub, call.channel ? 'ch ' + call.channel : '');
  // Change 065: a call that was not followed shows its channel time.
  const chan = call.not_followed && call.open_ms ? call.open_ms : 0;
  setText(c.dur, call.voice_ms ? dur(call.voice_ms) : chan ? dur(chan) : '—');
  setText(c.durSub, call.first_voice_ms != null ? 'first ' + call.first_voice_ms + ' ms' : chan ? 'channel time' : '');
  renderPlay(el, call);
  if (!c.details.hidden || !c.details.firstChild) {
    c.details.replaceChildren(...detailRows(call).flatMap(([k, v]) => [h('dt', { text: k }), h('dd', { text: v })]));
  }
}

export function callsList() {
  const c = card('Recent calls', { flush: true });
  const count = h('span', { class: 'dim', style: { fontSize: '0.86em' } });
  const nf = h('button', { class: 'chip', type: 'button', title: 'Encrypted and not-followed grants' }, 'Not followed');
  nf.addEventListener('click', () => setPref('showNotFollowed', !store.prefs.showNotFollowed));
  const search = h('input', { class: 'input', type: 'search', placeholder: 'Filter TG / source', 'aria-label': 'Filter calls' });
  search.addEventListener('input', () => render());
  c.right.append(count);
  const tools = h('div', { class: 'calls-tools' }, search, nf);
  const list = h('div', { class: 'calls' });
  const empty = h('div', { class: 'calls-empty', text: 'No calls yet.' });
  c.el.append(tools, list, empty);

  let timer = null;

  function render() {
    const data = store.calls;
    nf.classList.toggle('on', !!store.prefs.showNotFollowed);
    let items = data ? data.items : [];
    const q = search.value.trim().toLowerCase();
    if (q) {
      items = items.filter(x => [x.tg, x.tg_alias, x.source, x.source_alias, ...(x.sources || [])]
        .some(v => v !== null && v !== undefined && String(v).toLowerCase().includes(q)));
    }
    empty.hidden = items.length > 0;
    if (!data) setText(empty, 'Loading…');
    else setText(empty, q ? 'No matching calls.' : 'No calls yet.');
    const recs = data ? data.items.filter(x => x.recording).length : 0;
    setText(count, data ? data.items.length + ' calls · ' + recs + ' recorded' + (data.recording_enabled ? '' : ' · recording off') : '');
    keyedList(list, items, x => x.call_id, createRow, updateRow);
  }

  function update(kind) {
    if (kind === 'calls' || kind === 'prefs') render();
  }

  // Ages ("12 s ago") move without new data.
  timer = setInterval(() => { if (store.calls) render(); }, 5000);
  render();
  return { el: c.el, update, unmount: () => clearInterval(timer) };
}
