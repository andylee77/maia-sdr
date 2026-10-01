// Change 075: the DMR control channel's latest messages (Now view, DMR
// sites only), from /api/dmr/messages: grants, announcements, registrations,
// voice and link control, in SDRTrunk's text. The steady filler (ALOHA, IDLE,
// the SLC) is left out server-side; identical consecutive lines collapse into
// one with a count. Polled only while visible.

import { h, setText, card, switchInput } from '../dom.js';
import { clockMs } from '../format.js';
import { api } from '../api.js';

const POLL_MS = 2000;
const SHOW = 40;

// SDRTrunk class name -> a short tag and its colour class.
function tagOf(cls) {
  if (cls.includes('Grant')) return ['grant', 'c-grant'];
  if (cls.startsWith('Voice') || cls.includes('VoiceChannelUser') || cls === 'Terminator') return ['voice', 'c-traffic'];
  if (cls === 'Clear' || cls === 'Protect') return [cls.toLowerCase(), 'c-traffic'];
  if (cls.includes('Registration') || cls.includes('Acknowledge') || cls.includes('Ahoy') || cls.includes('RadioCheck')) return ['radio', 'c-recorder'];
  if (cls === 'SyncLossMessage') return ['sync', 'c-vocoder'];
  return ['cc', 'c-cc'];
}

export function dmrFeed() {
  const c = card('DMR control channel');
  const list = h('div', { class: 'log', style: { maxHeight: '40vh' } });
  const note = h('p', { class: 'card-note' });
  let showRoutine = false;
  const routine = switchInput('Routine broadcasts', false, on => { showRoutine = on; poll(); });
  c.body.append(h('div', { class: 'row', style: { marginBottom: '8px' } }, routine.el), list, note);
  c.el.hidden = true;

  // Announcements, call timers and vote-now advice repeat every few seconds.
  const ROUTINE = new Set(['CallTimerParameters', 'VoteNowAdvice', 'Announcement']);

  let timer = null;
  let active = false;
  async function poll() {
    if (!active || document.hidden) return;
    try {
      const r = await api.dmrMessages(500);
      const msgs = (r.messages || []).filter(m => showRoutine || !ROUTINE.has(m.class));
      // Collapse runs of the same text (a grant is sent twice, PROTECT many times).
      const rows = [];
      for (const m of msgs) {
        const last = rows[rows.length - 1];
        if (last && last.m.text === m.text && last.m.timeslot === m.timeslot) {
          last.n += 1;
          last.m = m;
        } else {
          rows.push({ m, n: 1 });
        }
      }
      const shown = rows.slice(-SHOW).reverse();
      list.replaceChildren(...shown.map(({ m, n }) => {
        const [tag, cls] = tagOf(m.class);
        return h('div', { class: 'log-line', title: m.class },
          h('span', { class: 't', text: clockMs(m.unix_ms) }),
          h('span', { class: 'c ' + cls, text: tag + (m.timeslot ? ' ' + m.timeslot : '') }),
          h('span', { class: 'm' + (m.valid ? '' : ' f'), text: m.text + (n > 1 ? '  ×' + n : '') }));
      }));
      if (!shown.length) list.replaceChildren(h('div', { class: 'log-line' }, h('span', { class: 'm f', text: showRoutine ? 'No messages yet.' : 'Nothing but routine broadcasts so far.' })));
      setText(note, 'Newest first. Column 2 is the message kind and timeslot. ALOHA, IDLE and the control channel SLC are not shown.');
    } catch {
      setText(note, 'DMR messages unavailable.');
    }
  }

  function update(s) {
    const dmr = s.site && s.site.protocol === 'dmr';
    if (dmr === active) return;
    active = dmr;
    c.el.hidden = !dmr;
    clearInterval(timer);
    if (dmr) {
      poll();
      timer = setInterval(poll, POLL_MS);
    }
  }

  return { el: c.el, update, unmount: () => clearInterval(timer) };
}
