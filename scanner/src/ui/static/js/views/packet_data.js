// Packet data on the Activity page, from /api/v1/data: the data channel, totals by kind, the radios
// with packet data and the recent packets. Network to radio only: radios transmit on the uplink,
// which the receiver does not cover. Kept in memory since the last start.

import { h, card, table } from '../dom.js';
import { num, unitLabel, DASH, dayTime } from '../format.js';

// What a data record is, in words.
const KINDS = {
  'packet/lrrp': 'Location (LRRP)',
  'packet/ars': 'Registration (ARS)',
  'packet/tms': 'Text message (TMS)',
  'packet/xcmp': 'Radio control (XCMP)',
  'packet/sndcp_control': 'Data session (SNDCP)',
  'packet/user_data': 'IP data',
  'packet/packet_data': 'IP data',
  'packet/encrypted_user_data': 'Encrypted data',
  'response/all_blocks_received': 'Delivery receipt',
  'response/selective_retry': 'Retry request',
  ambtc: 'Trunking (multi-block)',
  umbtc: 'Trunking (multi-block)',
};

function kindName(k) {
  return KINDS[k] || k.replace(/_/g, ' ');
}

function recordKind(r) {
  if (r.format === 'packet') return kindName('packet/' + (r.service || r.sap));
  if (r.format === 'response') return kindName('response/' + r.sap);
  return kindName(r.format);
}

export function packetDataCard() {
  const c = card('Packet data');

  function update(d) {
    const body = [];
    body.push(h('p', { class: 'card-note', style: { marginTop: '0' }, text:
      (d.data_channel_hz ? 'Data channel ' + (d.data_channel_hz / 1e6).toFixed(4) + ' MHz. ' : 'No data channel announced yet. ')
      + 'Network to radio only (radios transmit on the uplink, which is not received). Kept in memory since the last start.' }));
    const kinds = Object.entries(d.totals || {}).sort((a, b) => b[1] - a[1]);
    if (kinds.length) {
      body.push(h('div', { class: 'row', style: { gap: '6px', margin: '4px 0 10px' } },
        ...kinds.map(([k, n]) => h('span', { class: 'chip', text: kindName(k) + ' ' + n }))));
    }
    const radios = d.radios || [];
    body.push(h('h3', { text: 'Radios' }), radios.length
      ? table(['Radio', 'Packets', 'Kinds', 'IP', 'Last'], radios.slice(0, 25).map(r => h('tr', null,
        h('td', { text: unitLabel(r.llid, r.alias) }),
        h('td', { text: num(r.packets) }),
        h('td', { text: Object.entries(r.kinds || {}).sort((a, b) => b[1] - a[1]).slice(0, 2)
          .map(([k, n]) => kindName(k) + (n > 1 ? ' ×' + n : '')).join(', ') }),
        h('td', { text: r.ip || DASH }),
        h('td', { text: dayTime(r.last_ms) }))))
      : h('p', { class: 'card-note', text: 'No packet data yet.' }));
    const recent = d.recent || [];
    if (recent.length) {
      body.push(h('h3', { text: 'Recent' }), table(['Time', 'Kind', 'Radio', 'Detail', 'Size', ''], recent.slice(0, 20).map(r => h('tr', null,
        h('td', { text: dayTime(r.at_ms) }),
        h('td', { text: recordKind(r) }),
        h('td', { text: (r.outbound ? '→ ' : '← ') + unitLabel(r.llid, r.alias) }),
        h('td', { text: r.ip ? r.ip.src + ' → ' + r.ip.dst + (r.ip.dst_port ? ':' + r.ip.dst_port : '') : DASH }),
        h('td', { text: r.bytes ? r.bytes + ' B' : DASH }),
        h('td', null, r.crc_ok === false ? h('span', { class: 'badge bad', text: 'CRC' })
          : r.bad_blocks ? h('span', { class: 'badge hang', text: r.bad_blocks + ' bad' }) : '')))));
    }
    c.body.replaceChildren(...body);
  }

  return { el: c.el, update };
}
