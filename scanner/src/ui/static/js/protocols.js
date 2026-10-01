// What differs between protocols in the UI, in one place: generic pages ask the registry, never
// branch on the protocol themselves.

const hex = (v, width) => (v === null || v === undefined ? null : v.toString(16).toUpperCase().padStart(width, '0'));

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
};

const DMR = {
  label: 'DMR Tier III',
  systemIdentity: id => [['Model', id.model], ['Network', id.network]],
  siteIdentity: id => [['Site', id.site], ['Colour code', id.colour_code]],
  heardIdentity: id => [['Colour code', id.colour_code], ['Model', id.model], ['Network', id.network], ['Site', id.site]],
  okLabel: 'Messages passing checks',
  planLabel: 'Channels (LCN)',
};

const REGISTRY = { p25: P25, dmr_tier3: DMR };

const UNKNOWN = {
  label: '?', systemIdentity: () => [], siteIdentity: () => [], heardIdentity: () => [],
  okLabel: 'Messages passing', planLabel: 'Channel plan',
};

export function protocol(name) {
  return REGISTRY[name] || UNKNOWN;
}
