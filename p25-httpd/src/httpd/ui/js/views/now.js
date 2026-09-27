// "Now": what the radio is doing right now — control-channel health,
// the current call, and recent calls with recordings.

import { h, setText, card, switchInput, toast } from '../dom.js';
import { api } from '../api.js';
import { kick, refreshCalls } from '../store.js';
import { siteCard } from '../components/site_card.js';
import { callCard } from '../components/call_card.js';
import { callsList } from '../components/calls_list.js';

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
    if (!busy && document.activeElement !== sw.input) sw.input.checked = s.recording.enabled;
    setText(info, s.recording.enabled
      ? 'Every followed clear call is saved as a WAV; the newest ' + s.recording.max_count + ' are kept (' + s.recording.count + ' now, in RAM, lost on reboot).'
      : 'Recording is off: calls are still listed, without audio. ' + s.recording.count + ' earlier recordings kept.');
    const n = s.audio.listeners;
    setText(listen, n ? n + ' browser' + (n > 1 ? 's' : '') + ' listening live.' : 'Nobody listening live.');
  }
  return { el: c.el, update };
}

export function mount(host) {
  const call = callCard();
  const site = siteCard();
  const rec = recorderCard();
  const calls = callsList();
  call.el.classList.add('now-call');
  calls.el.classList.add('now-calls');
  host.append(
    h('div', { class: 'now-layout' },
      call.el,
      h('div', { class: 'stack now-side' }, site.el, rec.el),
      calls.el),
  );
  refreshCalls();
  return {
    update(kind, store) {
      call.update(kind, store);
      calls.update(kind, store);
      if (kind === 'state' && store.state) {
        site.update(store.state);
        rec.update(store.state);
      }
    },
    unmount() {
      call.unmount();
      calls.unmount();
    },
  };
}
