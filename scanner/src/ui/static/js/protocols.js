// What differs between protocols in the UI, in one place: generic pages ask the registry, never
// branch on the protocol themselves.

const hex = (v, width) => (v === null || v === undefined ? null : v.toString(16).toUpperCase().padStart(width, '0'));
const count = v => (v === null || v === undefined ? '—' : Math.round(v).toLocaleString());

const P25 = {
  label: 'P25',
  // Identity rows of a system and of a site.
  systemIdentity: id => [['WACN', hex(id.wacn, 5)], ['System', hex(id.system, 3)]],
  siteIdentity: id => [['RFSS', id.rfss], ['Site', id.site], ['NAC', hex(id.nac, 3)]],
  // The identity the control channel broadcasts.
  heardIdentity: id => [
    ['NAC', hex(id.nac, 3)], ['WACN', hex(id.wacn, 5)], ['System', hex(id.system, 3)],
    ['RFSS', id.rfss], ['Site', id.site],
  ],
  okLabel: 'TSBKs passing CRC',
  planLabel: 'Bands (IDEN_UP)',
  // The control channel's health beyond the common rows.
  healthRows: c => [
    ...(c.modulation ? [['Demodulator', c.modulation === 'c4fm' ? 'C4FM (software)' : 'LSM (gateware)']] : []),
    ...(c.tsbks_20s ? [['TSBKs in 20 s', `LSM ${count(c.tsbks_20s.lsm)}, C4FM ${count(c.tsbks_20s.c4fm)}`]] : []),
  ],
  // A found site's system (to group a scan's finds) and default names.
  scanSystem: id => ({ key: `p25:${id.wacn}-${id.system}`, label: `P25 WACN ${hex(id.wacn, 5)} system ${hex(id.system, 3)}` }),
  scanSiteName: id => `Site ${id.rfss}-${id.site}`,
  // What the site editor offers: the control channel's modulation; channel numbers by hand.
  edits: { modulation: true, lcnPlan: false, controlSlot: false },
};

const DMR = {
  label: 'DMR Tier III',
  systemIdentity: id => [['Model', id.model], ['Network', id.network]],
  siteIdentity: id => [['Site', id.site], ['Colour code', id.colour_code]],
  heardIdentity: id => [['Colour code', id.colour_code], ['Model', id.model], ['Network', id.network], ['Site', id.site]],
  okLabel: 'Messages passing checks',
  planLabel: 'Channels (LCN)',
  healthRows: c => (c.carrier_offset_hz === null || c.carrier_offset_hz === undefined ? [] : [['Carrier offset', `${count(c.carrier_offset_hz)} Hz`]]),
  scanSystem: id => ({ key: `dmr:${id.model}-${id.network}`, label: `DMR ${id.model} network ${id.network}` }),
  scanSiteName: id => `Site ${id.site}`,
  edits: { modulation: false, lcnPlan: true, controlSlot: true },
};

const REGISTRY = { p25: P25, dmr_tier3: DMR };

const UNKNOWN = {
  label: '?', systemIdentity: () => [], siteIdentity: () => [], heardIdentity: () => [],
  okLabel: 'Messages passing', planLabel: 'Channel plan',
  healthRows: () => [],
  scanSystem: () => ({ key: '?', label: 'Unknown system' }),
  scanSiteName: () => 'Site',
  edits: { modulation: false, lcnPlan: false, controlSlot: false },
};

export function protocol(name) {
  return REGISTRY[name] || UNKNOWN;
}

// "P25 and DMR Tier III": the protocols the radio knows.
export function protocolNames() {
  const labels = Object.values(REGISTRY).map(p => p.label);
  return labels.length > 1 ? `${labels.slice(0, -1).join(', ')} and ${labels[labels.length - 1]}` : labels.join('');
}

// What the crystal correction is measured from at the live site.
const CRYSTAL_SOURCE = {
  p25_loop: 'the P25 carrier loop',
  dmr_equaliser: 'the DMR equaliser',
  spectrum: 'the spectrum only (C4FM)',
};

export function crystalSource(source) {
  return CRYSTAL_SOURCE[source] || source;
}
