// A recording's player: play and pause, its position and its time. A recording has no volume of
// its own: every one plays at the header's volume, as live audio does (setRecordingVolume).

import { h, icon, setText } from '../dom.js';

let volume = 1;
// The players' audio, held weakly: a row the list drops takes its player with it.
const players = new Set();

// The header's volume, 0 to 1.
export function setRecordingVolume(v) {
  volume = Math.max(0, Math.min(1, Number(v) || 0));
  for (const ref of players) {
    const audio = ref.deref();
    if (audio) audio.volume = volume;
    else players.delete(ref);
  }
}

// "1:07".
function clockText(s) {
  const t = Number.isFinite(s) ? Math.floor(s) : 0;
  return `${Math.floor(t / 60)}:${String(t % 60).padStart(2, '0')}`;
}

// The recording at `url`: `el` to show, `play(at)` to play it (from `at` seconds when given) and
// `pause()` (an element taken out of the page plays on otherwise).
export function recordingPlayer(url) {
  const audio = h('audio', { preload: 'auto', src: url });
  audio.volume = volume;
  players.add(new WeakRef(audio));
  const toggle = h('button', { class: 'btn small icon', type: 'button' });
  const seek = h('input', { class: 'seek', type: 'range', min: '0', max: '0', step: '0.01', value: '0', 'aria-label': 'Position' });
  const time = h('span', { class: 'rec-time', text: '0:00 / 0:00' });
  // The slider follows playback except while it is held.
  let held = false;

  const showState = () => {
    const label = audio.paused ? 'Play' : 'Pause';
    toggle.replaceChildren(icon(audio.paused ? 'play' : 'pause'));
    toggle.title = label;
    toggle.setAttribute('aria-label', label);
  };
  const showPosition = () => {
    if (!held) seek.value = String(audio.currentTime);
    setText(time, `${clockText(audio.currentTime)} / ${clockText(audio.duration)}`);
  };
  const follow = () => {
    showPosition();
    if (!audio.paused) requestAnimationFrame(follow);
  };
  audio.addEventListener('loadedmetadata', () => {
    seek.max = String(audio.duration || 0);
    showPosition();
  });
  audio.addEventListener('play', () => {
    showState();
    requestAnimationFrame(follow);
  });
  audio.addEventListener('pause', showState);
  audio.addEventListener('seeked', showPosition);
  audio.addEventListener('ended', showPosition);
  seek.addEventListener('pointerdown', () => { held = true; });
  for (const ev of ['pointerup', 'pointercancel', 'change']) seek.addEventListener(ev, () => { held = false; });
  seek.addEventListener('input', () => { audio.currentTime = Number(seek.value); });

  const play = at => {
    const from = at !== null && at !== undefined;
    const go = () => {
      if (from) audio.currentTime = at;
      audio.play().catch(() => { /* some phones need another tap */ });
    };
    if (from && audio.readyState < 1) audio.addEventListener('loadedmetadata', go, { once: true });
    else go();
  };
  toggle.addEventListener('click', () => (audio.paused ? play(null) : audio.pause()));
  showState();
  return { el: h('span', { class: 'rec-player' }, toggle, seek, time, audio), play, pause: () => audio.pause() };
}
