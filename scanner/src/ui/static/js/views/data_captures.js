// Data's Captures page: the IQ of the bursts kept on the card, each as two files to download:
// the samples and their SigMF description. A capture is asked for on Scan ("Keep IQ"); the list
// is read again when one is written.

import { h, card, table, toast, setText, icon, iconButton } from '../dom.js';
import { api } from '../api.js';
import { want, store } from '../store.js';
import { mhz, bytes, num, dayTime } from '../format.js';
import { readingText } from './data_parts.js';

const FILE = '/api/v1/data/captures/';
const plural = (n, one) => `${num(n)} ${one}${n === 1 ? '' : 's'}`;

function link(name, text, title) {
  return h('a', { class: 'btn small', href: FILE + encodeURIComponent(name), download: name, title }, icon('download', 14), h('span', { text }));
}

export function mount(el) {
  let gone = false;
  // The captures written when the list was last read, the list, and the unit it is shown in.
  let written = null;
  let listing = null;
  let unit = store.units.temperature;

  const c = card('Captures');
  const totals = h('p', { class: 'card-note data-totals' });
  const rows = h('div', { class: 'data-table' });
  c.body.append(
    totals,
    rows,
    h('p', { class: 'card-note', text: 'A capture is a burst\'s IQ as the radio heard it: 250 kSPS, 16-bit I and Q (.cs16), 20 ms before the burst and after it, centred on the frequency in its name. The description (.sigmf-meta, SigMF) says when and where it was taken, what the burst was, and what it said when the radio read it. On a PC: inspectrum and URH open the samples; rtl_433 reads them with -r.' }));
  el.append(h('div', { class: 'data-page' },
    h('p', { class: 'card-note files-where' }, 'The IQ of bursts, kept when asked on ', h('a', { href: '#scan', text: 'Scan' }), ' ("Keep IQ"). The card keeps the newest of them.'),
    c.el));

  async function remove(k) {
    if (!confirm(`Delete the capture of ${dayTime(k.unix_ms)} at ${mhz(k.freq_hz, 3)}? Both its files go.`)) return;
    try {
      draw(await api.dataDeleteCapture(k.id));
    } catch (e) {
      toast(e.message, true);
    }
  }

  function draw(l) {
    if (gone) return;
    setText(totals, l.count ? `${plural(l.count, 'capture')}, ${bytes(l.bytes)}; the newest ${num(l.max_count)} or ${bytes(l.max_bytes)} are kept.` : 'None yet.');
    listing = l;
    rows.replaceChildren(...(l.items.length ? [table(['Taken', 'Burst at', 'Length', 'Over the floor', 'Decoded', 'Size', 'Files', ''], l.items.map(k => h('tr', null,
      h('td', { class: 'num', title: k.id, text: dayTime(k.unix_ms) }),
      h('td', { class: 'num', text: mhz(k.freq_hz, 3) }),
      h('td', { class: 'num', text: `${num(k.length_ms)} ms` }),
      h('td', { class: 'num', text: `${k.over_db.toFixed(1)} dB` }),
      h('td', { text: k.reading ? `${k.reading.model}: ${readingText(k.reading)}` : '' }),
      h('td', { class: 'num', text: bytes(k.bytes) }),
      h('td', { class: 'data-files' }, link(k.file, 'IQ', k.file), link(k.meta, 'Description', k.meta)),
      h('td', { class: 'files-get' }, iconButton('trash', 'Delete this capture', { onclick: () => remove(k) })))))] : []));
  }

  async function read() {
    try {
      draw(await api.dataCaptures());
    } catch (e) {
      // A radio without the folder has none to list.
      if (!gone) setText(totals, e.status === 404 ? 'This radio has nowhere to keep captures.' : e.message);
    }
  }

  // A capture written since the list was read: the list again.
  const stop = want({ data: true }, m => {
    const n = m.data.counts.captures;
    if (written !== null && n !== written) read();
    written = n;
  });
  read();

  return {
    // A temperature follows the unit the radio's Settings picks.
    update(s) {
      if (s.units.temperature !== unit && listing) {
        unit = s.units.temperature;
        draw(listing);
      }
    },
    unmount() {
      gone = true;
      stop();
    },
  };
}
