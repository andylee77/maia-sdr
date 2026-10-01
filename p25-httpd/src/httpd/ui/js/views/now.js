// "Now": what the radio is doing right now — control-channel health,
// the current call, and recent calls with recordings.

import { h, setText, card, switchInput, toast } from '../dom.js';
import { api } from '../api.js';
import { kick, refreshCalls, store as store_ } from '../store.js';
import { siteCard } from '../components/site_card.js';
import { callCard } from '../components/call_card.js';
import { callsList } from '../components/calls_list.js';
import { speakersPanel } from '../components/speakers_panel.js';

function recorderCard() {
  const c = card('Recording');
  const info = h('div', { class: 'dim', style: { fontSize: '0.9em' } });
  let busy = false;
  const sw = switchInput('Save calls', true, async on => {
    if (busy) return;
    busy = true;
    try {
      const r = await api.putSettings({ recording: { enabled: on } });
      toast((on ? 'Recording on' : 'Recording off') + (r.persisted ? '' : ' (not saved to flash)'), !r.persisted);
      kick(50);
    } catch (e) {
      sw.input.checked = !on;
      toast('Could not change recording: ' + e.message, true);
    } finally {
      busy = false;
    }
  });
  const listen = h('div', { class: 'dim', style: { fontSize: '0.9em', marginTop: '8px' } });
  c.body.append(sw.el, h('div', { style: { marginTop: '8px' } }, info), listen);

  function update(s) {
    const r = s.recording;
    if (!busy && document.activeElement !== sw.input) sw.input.checked = r.enabled;
    // Change 057: RAM (lost on reboot) or the SD card (kept).
    const sd = r.storage === 'sd';
    const sdBad = sd && r.sd_state && r.sd_state !== 'ok' && r.sd_state !== 'unknown';
    const where = sd
      ? (sdBad ? 'SD card selected but ' + r.sd_state.replace('_', ' ') + ': saving to RAM' : 'on the SD card')
      : 'in RAM, lost on reboot';
    const held = r.count + ' now' + (r.sd_count && r.ram_count ? ' (' + r.sd_count + ' on SD, ' + r.ram_count + ' in RAM)' : '');
    setText(info, r.enabled
      ? 'Every followed clear call is saved as a WAV ' + where + '. ' + held + '.'
      : 'Recording is off: calls are still listed, without audio. ' + r.count + ' earlier recordings kept.');
    info.style.color = sdBad ? 'var(--warn)' : '';
    const n = s.audio.listeners;
    setText(listen, n ? n + ' browser' + (n > 1 ? 's' : '') + ' listening live.' : 'Nobody listening live.');
  }
  return { el: c.el, update };
}

export function mount(host) {
  // Change 066: a card per traffic chain (chain 2's hides with one).
  const call = callCard(1);
  const call2 = callCard(2);
  const site = siteCard();
  const rec = recorderCard();
  const calls = callsList();
  const speakers = speakersPanel();
  calls.el.classList.add('now-calls');
  host.append(
    h('div', { class: 'now-layout' },
      h('div', { class: 'stack now-call' }, call.el, call2.el),
      h('div', { class: 'stack now-side' }, speakers.el, site.el, rec.el),
      calls.el),
  );
  refreshCalls();
  if (store_.settings) speakers.update('settings', store_);
  return {
    update(kind, store) {
      call.update(kind, store);
      call2.update(kind, store);
      calls.update(kind, store);
      speakers.update(kind, store);
      if (kind === 'state' && store.state) {
        site.update(store.state);
        rec.update(store.state);
      }
    },
    unmount() {
      call.unmount();
      call2.unmount();
      site.unmount();
      calls.unmount();
    },
  };
}
