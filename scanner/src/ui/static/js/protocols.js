// What differs between protocols in the UI, in one place: generic pages ask the registry, never
// branch on the protocol themselves.

const hex = (v, width) => (v === null || v === undefined ? null : v.toString(16).toUpperCase().padStart(width, '0'));
const count = v => (v === null || v === undefined ? '—' : Math.round(v).toLocaleString());
const known = v => v !== null && v !== undefined;
// RadioReference's "001 · 1": decimal, then hex.
const decHex = (v, width = 1) => (known(v) ? `${String(v).padStart(width, '0')} · ${hex(v, 1)}` : null);

// Typed identity fields back to numbers.
const hexIn = digits => text => {
  if (!new RegExp(`^[0-9a-f]{1,${digits}}$`, 'i').test(text)) throw new Error(`is ${digits} hex digits at most`);
  return parseInt(text, 16);
};
const decIn = (max = Number.MAX_SAFE_INTEGER) => text => {
  if (!/^\d+$/.test(text) || Number(text) > max) throw new Error(`is a whole number up to ${max}`);
  return Number(text);
};

const P25 = {
  label: 'P25',
  // What a new system's "System type" starts as.
  systemType: 'Project 25',
  // Identity rows of a system and of a site.
  systemIdentity: id => [['System ID', `Sysid ${hex(id.system, 3) ?? '?'} · WACN ${hex(id.wacn, 5) ?? '?'}`]],
  siteIdentity: id => [['RFSS', decHex(id.rfss)], ['Site', decHex(id.site, 3)], ['NAC', hex(id.nac, 3)]],
  // The editors' identity fields (`hint` shows what was typed another way).
  identityEdits: [
    { key: 'system', label: 'Sysid', show: v => hex(v, 3), parse: hexIn(3) },
    { key: 'wacn', label: 'WACN', show: v => hex(v, 5), parse: hexIn(5) },
  ],
  siteIdentityEdits: [
    { key: 'rfss', label: 'RFSS', show: String, parse: decIn(255), hint: v => `· ${hex(v, 1)}` },
    { key: 'site', label: 'Site', show: v => String(v).padStart(3, '0'), parse: decIn(255), hint: v => `· ${hex(v, 1)}` },
    { key: 'nac', label: 'NAC', show: v => hex(v, 3), parse: hexIn(3) },
  ],
  // A found site's identity as a configured system and site keep it.
  heardSystem: id => ({ wacn: id.wacn, system: id.system }),
  heardSite: id => ({ rfss: id.rfss, site: id.site, nac: id.nac, lra: id.lra }),
  sameSystem: (configured, heard) => known(configured.wacn) && configured.wacn === heard.wacn && configured.system === heard.system,
  // What tells a system's sites apart, in a configured or a heard identity.
  siteKey: id => (known(id.rfss) && known(id.site) ? `${id.rfss}-${id.site}` : null),
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
  scanSystem: id => ({ key: `p25:${id.wacn}-${id.system}`, label: `P25 system ${hex(id.system, 3)}` }),
  scanSiteName: id => `Site ${String(id.site).padStart(3, '0')}`,
  // What the site editor offers: the control channel's modulation; channel numbers by hand.
  edits: { modulation: true, lcnPlan: false, controlSlot: false },
};

const DMR = {
  label: 'DMR Tier III',
  systemType: 'DMR Tier III',
  systemIdentity: id => [['System ID', `Model ${id.model ?? '?'} · Network ${id.network ?? '?'}`]],
  siteIdentity: id => [['Site', id.site], ['Colour code', id.colour_code]],
  identityEdits: [
    { key: 'model', label: 'Model', options: ['tiny', 'small', 'large', 'huge'] },
    { key: 'network', label: 'Network', show: String, parse: decIn() },
  ],
  siteIdentityEdits: [
    { key: 'site', label: 'Site', show: String, parse: decIn() },
    { key: 'colour_code', label: 'Colour code', show: String, parse: decIn(15) },
  ],
  heardSystem: id => ({ model: id.model.toLowerCase(), network: id.network }),
  heardSite: id => ({ site: id.site, colour_code: id.colour_code }),
  sameSystem: (configured, heard) => known(configured.model) && configured.model === heard.model.toLowerCase() && configured.network === heard.network,
  siteKey: id => (known(id.site) && known(id.colour_code) ? `${id.site}-${id.colour_code}` : null),
  heardIdentity: id => [['Colour code', id.colour_code], ['Model', id.model], ['Network', id.network], ['Site', id.site]],
  okLabel: 'Messages passing checks',
  planLabel: 'Channels (LCN)',
  healthRows: c => (c.carrier_offset_hz === null || c.carrier_offset_hz === undefined ? [] : [['Carrier offset', `${count(c.carrier_offset_hz)} Hz`]]),
  scanSystem: id => ({ key: `dmr:${id.model}-${id.network}`, label: `DMR ${id.model.toLowerCase()} network ${id.network}` }),
  scanSiteName: id => `Site ${id.site}`,
  edits: { modulation: false, lcnPlan: true, controlSlot: true },
};

const REGISTRY = { p25: P25, dmr_tier3: DMR };

const UNKNOWN = {
  label: '?', systemType: '', systemIdentity: () => [], siteIdentity: () => [], heardIdentity: () => [],
  identityEdits: [], siteIdentityEdits: [],
  heardSystem: () => ({}), heardSite: () => ({}), sameSystem: () => false, siteKey: () => null,
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
