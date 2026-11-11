// TV's diagnostics, filling the screen: what TV did (each TV scan and what it named, live TV
// starting), and down the right the 8-VSB receiver while a channel plays live: its state, the
// block's registers, its loops at the newest field, and what it decoded and lost. The radio
// pushes the receiver's numbers while the page is open. The hardware and the radio's own log are
// the rail's Diagnostics.

import { h } from '../dom.js';
import { mhz, ago, num, DASH } from '../format.js';
import { want, boardNow } from '../store.js';
import { kvCard, mainPane, elsewhere } from './diag_parts.js';

// Kept while the page is left for another.
const state = { tab: 'events' };

const STATE = { idle: 'Idle: no channel plays live', starting: 'Starting', searching: 'Finding the signal', playing: 'Playing', error: 'Failed' };
const known = v => v !== null && v !== undefined;
const yes = v => (v ? 'yes' : 'no');
// A signed value at `digits` decimals; one that rounds to nothing is 0, not -0.
const signed = (v, digits = 0) => num(Math.abs(v) < 0.5 / 10 ** digits ? 0 : v, digits);
// A count's share of the codewords.
const share = (n, of) => (of ? `${num(n)} (${num((100 * n) / of, 2)} %)` : num(n));

export function mount(el) {
  const receiver = kvCard('Receiver');
  const block = kvCard('8-VSB block');
  const loops = kvCard('Loops');
  const decode = kvCard('Decoding');
  const main = mainPane('tv', state, { label: 'TV\'s events' });

  el.append(h('div', { class: 'diag no-plot' }, main.el,
    h('div', { class: 'diag-side' }, receiver.el, block.el, loops.el, decode.el,
      elsewhere('The hardware as the radio reads it back, and the radio\'s own log, are under ', h('a', { href: '#diag', text: 'Diagnostics' }), ' in the rail.'))));

  function show(s) {
    const on = s.state !== 'idle';
    receiver.show(s.available ? [
      ['State', STATE[s.state] || s.state],
      ...(s.error ? [['Error', s.error]] : []),
      ['Channel', known(s.rf) ? `RF ${s.rf}` : DASH],
      ['Carrier starts at', on ? `${signed(s.offset_hz)} Hz off the centre` : DASH],
      ['Started', on && s.started_unix_ms ? ago(boardNow() - s.started_unix_ms) : DASH],
      ['Station', known(s.psip.tsid) ? `TSID ${s.psip.tsid}, ${num(s.psip.channels.length)} channel${s.psip.channels.length === 1 ? '' : 's'}` : DASH],
      ['On the stream', `${num(s.viewers)} player${s.viewers === 1 ? '' : 's'}`],
    ] : null, 'This radio core has no 8-VSB block: live TV does not play here.');
    const b = s.block;
    block.show(b ? [
      ['Enabled', yes(b.enabled)],
      ['Segment syncs', b.segments ? 'found' : 'not found'],
      ['Field syncs', b.fields ? `counted, ${num(b.fields_counted)} since the enable` : 'not counted'],
      ['Timing loop', b.tracking ? 'tracking' : 'acquiring'],
      ['Equalizer taps', `${num(b.taps_loaded)} sets loaded${b.taps_armed ? ', one armed' : ''}`],
      ['Snapshot', b.snapshot_pending ? 'pending' : 'none pending'],
      ['Matched filter', b.overrun ? 'overrun: the block restarts' : 'keeping up'],
      ['Buffers refused', num(b.lost)],
    ] : null, s.available ? 'The block is read while a channel plays live.' : 'No block.');
    loops.show(known(s.field) ? [
      ['Field', num(s.field)],
      ['Carrier', known(s.carrier_hz) ? `${signed(s.carrier_hz)} Hz off the centre` : DASH],
      ['Timing', known(s.timing_ppm) ? `${signed(s.timing_ppm, 1)} ppm` : DASH],
      ['Gain word', known(s.gain_word) ? num(s.gain_word) : DASH],
      ['Segment sync', known(s.sync) ? `${num(s.sync)} of 2,560` : DASH],
      ['MER', s.mer_db.length ? `${num(s.mer_db[s.mer_db.length - 1], 1)} dB (${num(Math.min(...s.mer_db), 1)} to ${num(Math.max(...s.mer_db), 1)} over ${s.mer_db.length} fields)` : DASH],
      ['Decoder', s.rerun ? 'started afresh after its fields were lost' : 'as started'],
    ] : null, 'No field yet.');
    decode.show(on || s.codewords ? [
      ['Codewords', num(s.codewords)],
      ['Corrected', share(s.corrected, s.codewords)],
      ['Beyond Reed-Solomon', share(s.failed, s.codewords)],
      ['Packets read', num(s.packets)],
      ['Packets lost', `${num(s.missed)} missed, ${num(s.dropped)} dropped`],
      ['Snapshots', num(s.snapshots)],
      ['Equalizer solves', known(s.solve_ms) ? `${num(s.solves)}, the last in ${num(s.solve_ms)} ms` : num(s.solves)],
      ['Restarts', num(s.restarts)],
    ] : null, 'Nothing decoded: no channel plays live.');
  }

  const stop = want({ atsc_live: true }, m => show(m.live));
  return {
    update() {},
    unmount() {
      main.stop();
      stop();
    },
  };
}
