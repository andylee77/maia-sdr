// The board card (Diagnostics): what the hardware holds now, read back every few seconds: the
// AD9361 (LO, gain, RSSI), the control chain and each lane (NCO offset, the gateware decoder).

import { h, card, setText } from '../dom.js';
import { mhz, DASH } from '../format.js';
import { api } from '../api.js';

const EVERY_MS = 3000;

const hz = v => (v === null || v === undefined ? DASH : `${Math.round(v).toLocaleString()} Hz`);
const onOff = v => (v ? 'on' : 'off');

function lsm(c) {
  return c ? `${onOff(c.enable)}; dibit DMA ${onOff(c.dibit_dma)}, DC block ${onOff(c.dc_block)}, AGC ${onOff(c.agc)}` : DASH;
}

export function boardCard() {
  const c = card('Board');
  const body = h('div');
  const note = h('p', { class: 'card-note', text: ' ' });
  c.body.append(body, note);
  let timer = null;

  async function poll() {
    try {
      const r = await api.radio();
      const b = r.readback || {};
      const t = r.tuning;
      const st = b.control_status;
      const nid = b.control_nid ? `0x${b.control_nid[0].toString(16).toUpperCase()} / ${b.control_nid[1]}` : DASH;
      const rows = [
        ['Gateware', r.hardware.core_version || DASH],
        ['Lanes', String(r.hardware.lanes)],
        ['AD9361 LO', `${mhz(b.lo_hz, 6)} (nominal ${mhz(t.lo_hz, 6)}, shift ${t.lo_shift_hz} Hz)`],
        ['Gain', `${b.gain_mode || DASH}, ${b.gain_db ?? DASH} dB; RSSI ${b.rssi_db ?? DASH} dB`],
        ['Control NCO', hz(b.control_nco_hz)],
        ['Control decoder', lsm(b.control_lsm)],
        ['Last frame ID', st ? `${nid}; ${st.n_errors} bit errors, sync distance ${st.sync_distance}${st.dibit_overflow ? ', dibit overflow' : ''}` : nid],
        ...[0, 1].filter(i => i < r.hardware.lanes).flatMap(i => [
          [`Lane ${i + 1} NCO`, hz(b.lane_nco_hz && b.lane_nco_hz[i])],
          [`Lane ${i + 1} decoder`, lsm(b.lane_lsm && b.lane_lsm[i])],
        ]),
      ];
      body.replaceChildren(h('table', { class: 'kv' }, ...rows.map(([k, v]) => h('tr', null, h('th', { text: k }), h('td', { text: v })))));
      setText(note, ' ');
    } catch (e) {
      setText(note, e.message);
    }
    timer = setTimeout(poll, EVERY_MS);
  }

  poll();
  return { el: c.el, stop() { clearTimeout(timer); } };
}
