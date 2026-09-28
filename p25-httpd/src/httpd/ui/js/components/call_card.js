// "Now on air" card: the call the lifecycle holds open, or idle.
//
// Phases (see app/ui_state.rs): acquiring = granted, no voice yet;
// voice = voice within the last 1.5 s; hang = silent, the call closes
// `close_in_ms` from now unless voice or a CC update arrives. The
// pre-056 dashboard showed hang time as an active call.
//
// Change 057: `close_via` says which close is pending — "end" (the end
// of the transmission was decoded; closes after the end grace, CC
// updates do not extend it) or "timeout" (no keep-alive for hang_ms).
//
// Change 066: one card per traffic chain. With two chains the cards are
// titled by speaker (chain 1 follows the left groups, chain 2 the
// right); chain 2's card hides while only one chain runs.

import { h, setText, card } from '../dom.js';
import { mhz, dur, ago, tgLabel, unitLabel } from '../format.js';
import { boardNow } from '../store.js';
import { metric } from './kv_table.js';

const VOICE_HOLD_MS = 1500;
const ACQUIRE_WINDOW_MS = 3000;
const PHASE_TEXT = { voice: 'Voice', hang: 'Hang', acquiring: 'Acquiring' };

function localPhase(call, dt) {
  const now = call.started_unix_ms + call.elapsed_ms + dt;
  if (call.close_via === 'end') return 'hang';
  if (call.last_voice_unix_ms && now - call.last_voice_unix_ms <= VOICE_HOLD_MS) return 'voice';
  if (!call.last_voice_unix_ms && call.elapsed_ms + dt <= ACQUIRE_WINDOW_MS) return 'acquiring';
  return 'hang';
}

const TITLES = { 1: 'Left speaker · chain 1', 2: 'Right speaker · chain 2' };

// The open call of `chain` (1 or 2) and that chain's state.
function callOf(state, chain) {
  if (!state) return null;
  if (state.calls) return state.calls.find(x => x.chain === chain) || null;
  return chain === 1 ? state.call : null;
}

function chainOf(state, chain) {
  if (!state) return null;
  if (state.chains) return state.chains.find(x => x.number === chain) || null;
  return chain === 1 ? state.chain : null;
}

export function callCard(chain = 1) {
  const c = card('Now on air', { class: 'call-card' });
  const title = c.head.querySelector('h2');
  const phase = h('span', { class: 'badge' });
  const enc = h('span', { class: 'badge enc', text: 'Encrypted', hidden: true });
  const rec = h('span', { class: 'badge rec', text: '● Rec', hidden: true, title: 'A recording is being written' });
  c.right.append(enc, rec, phase);

  const tg = h('div', { class: 'call-tg' });
  const alias = h('div', { class: 'call-alias' });
  const src = h('div', { class: 'call-src' });
  const m = { freq: metric('Frequency'), elapsed: metric('Elapsed'), voice: metric('Voice'), close: metric('Closes in') };
  const active = h('div', null, tg, alias, src, h('div', { class: 'call-facts' }, Object.values(m).map(x => x.el)));
  const idleTitle = h('div', { class: 'big', text: 'No active call' });
  const idleText = h('div');
  const idle = h('div', { class: 'call-idle' }, idleTitle, idleText);
  c.body.append(active, idle);

  let state = null;
  let lastCall = null;
  let timer = null;

  function render() {
    const chains = state && state.chains ? state.chains.length : 1;
    c.el.hidden = chain > chains;
    setText(title, chains > 1 ? TITLES[chain] : 'Now on air');
    const call = callOf(state, chain);
    active.hidden = !call;
    idle.hidden = !!call;
    c.el.classList.remove('voice', 'hang', 'acquiring');
    if (!call) {
      phase.hidden = true; enc.hidden = true; rec.hidden = true;
      renderIdle();
      return;
    }
    const dt = Math.max(0, boardNow() - state.now_unix_ms);
    const ph = localPhase(call, dt);
    c.el.classList.add(ph);
    phase.hidden = false;
    phase.className = 'badge ' + ph;
    const ended = call.close_via === 'end';
    setText(phase, ended ? 'Ended' : PHASE_TEXT[ph]);
    phase.title = ended ? 'End of transmission decoded' + (call.end_lc ? ' (' + call.end_lc.replace(/_/g, ' ') + ')' : '') : '';
    enc.hidden = !call.encrypted;
    rec.hidden = !call.recording;

    setText(tg, call.tg_alias ? call.tg_alias : 'TG ' + call.tg);
    setText(alias, call.tg_alias ? 'TG ' + call.tg : '');
    setText(src, 'From ' + unitLabel(call.source, call.source_alias));
    m.freq.set(mhz(call.freq_hz) + (call.channel ? ' · ch ' + call.channel : ''));
    m.elapsed.set(dur(call.elapsed_ms + dt));
    m.voice.set(call.voice_ms ? dur(call.voice_ms) : 'none yet');
    const closeIn = Math.max(0, call.close_in_ms - dt);
    m.close.set(ph === 'hang' ? dur(closeIn) : '—');
  }

  function renderIdle() {
    const ch = chainOf(state, chain);
    const parts = [];
    if (ch && ch.parked_freq_hz) parts.push('Traffic chain parked on ' + mhz(ch.parked_freq_hz) + '.');
    if (ch && !ch.follower_enabled) parts.push('Grant follower is OFF.');
    if (lastCall) {
      const age = Math.max(0, boardNow() - lastCall.ended_unix_ms);
      parts.push('Last call ' + tgLabel(lastCall.tg, lastCall.tg_alias) + ' from '
        + unitLabel(lastCall.source, lastCall.source_alias) + ', ended ' + ago(age) + '.');
    }
    setText(idleText, parts.join(' ') || 'Waiting for a grant on the control channel.');
  }

  function update(kind, store) {
    if (kind === 'state') state = store.state;
    if (kind === 'calls' && store.calls) {
      // Change 066: this chain's last call (older summaries carry no
      // chain: chain 1's).
      // Change 073: only the active site's (the list may show another).
      const site = store.state && store.state.site ? store.state.site.name : null;
      lastCall = (store.calls.items || [])
        .find(x => !x.not_followed && (x.chain || 1) === chain && (!site || !x.site || x.site === site)) || null;
    }
    render();
  }

  timer = setInterval(render, 250);
  return { el: c.el, update, unmount: () => clearInterval(timer) };
}
