// The board card (Diagnostics): what the hardware holds now, as the radio reads it back every few
// seconds: the AD9361 (LO, gain, RSSI), the radio core's lanes (NCO offset, packets, tag), its
// lane ring and the packets each lane delivered.

import { h, card } from '../dom.js';
import { mhz, DASH } from '../format.js';
import { want } from '../store.js';

const hz = v => (v === null || v === undefined ? DASH : `${Math.round(v).toLocaleString()} Hz`);

function lane(l) {
  return l ? `${hz(l.nco_hz)}; packets ${l.packets ? 'on' : 'off'}, tag ${l.tag}` : DASH;
}

function packets(c) {
  return c ? `${c.packets} (${c.stale} stale, ${c.settling} samples settling, ${c.lost} after lost samples, ${c.missed} missed)` : DASH;
}

export function boardCard() {
  const c = card('Board');
  const body = h('div', null, h('p', { class: 'dim', text: 'Reading the hardware.' }));
  c.body.append(body);

  function show(r) {
    const b = r.readback || {};
    const t = r.tuning;
    const counters = b.lane_counters || [];
    const ring = b.ring;
    const rows = [
      ['Radio core', r.hardware.core_version || DASH],
      ['Traffic lanes', String(r.hardware.lanes)],
      ['AD9361 LO', `${mhz(b.lo_hz, 6)} (nominal ${mhz(t.lo_hz, 6)}, shift ${t.lo_shift_hz} Hz)`],
      ['Gain', `${b.gain_mode || DASH}, ${b.gain_db ?? DASH} dB; RSSI ${b.rssi_db ?? DASH} dB`],
      ['ADC clips', b.adc_clips === null || b.adc_clips === undefined ? DASH : `${b.adc_clips} of ${b.sample_count ?? DASH} samples`],
      ['Lane ring', ring ? `${ring.enabled ? 'on' : 'off'}, sub-buffer ${ring.last_buffer} done; ${b.packet_faults ?? 0} packets refused` : DASH],
      ['Control lane', lane(b.control)],
      ['Control packets', packets(counters[0])],
      ...[0, 1].filter(i => i < r.hardware.lanes).flatMap(i => [
        [`Lane ${i + 1}`, lane(b.lanes && b.lanes[i])],
        [`Lane ${i + 1} packets`, packets(counters[i + 1])],
      ]),
    ];
    body.replaceChildren(h('table', { class: 'kv' }, ...rows.map(([k, v]) => h('tr', null, h('th', { text: k }), h('td', { text: v })))));
  }

  const stop = want({ radio: true }, m => show(m.radio));
  return { el: c.el, stop };
}
