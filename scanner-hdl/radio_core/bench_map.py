#
# Fishball radio core - the bench's register map (schema fbench.regmap/1), from the core's own
# register banks: what the board agent may read and write, which reads clear sticky bits, and
# each bank's clock domain. The SVD carries none of the last two.
#
# SPDX-License-Identifier: MIT
#

import argparse
import json
import os

from maia_hdl.register import Access

from .p25_top import ADDRESS_WIDTH, PRODUCT_ID, P25Core

# The core's AXI-Lite window (the UIO device p25-core).
BASE = 0x7C46_0000

_FIELD_ACCESS = {Access.R: 'ro', Access.Rsticky: 'ro', Access.RW: 'rw', Access.W: 'wo',
                 Access.Wpulse: 'wo'}


def _register(name, offset, fields, domain, expected=None):
    readable = any(f.access in (Access.R, Access.RW, Access.Rsticky) for f in fields)
    writable = any(f.access in (Access.RW, Access.W, Access.Wpulse) for f in fields)
    sticky = any(f.access == Access.Rsticky for f in fields)
    out_fields = []
    lsb = 0
    for f in fields:
        out_fields.append({'name': f.name, 'lsb': lsb, 'width': f.width,
                           'access': _FIELD_ACCESS[f.access], 'desc': f.name})
        lsb += f.width
    return {
        'name': name,
        'offset': f'0x{offset:03X}',
        'access': 'rw' if readable and writable else ('ro' if readable else 'wo'),
        'width': 32,
        'reset': None,
        'snapshot': None,
        'desc': name + (' (a read clears its sticky bits)' if sticky else ''),
        'domain': domain,
        'read_side_effect': sticky,
        'expected': expected,
        'fields': out_fields,
    }


def bench_map(core=None):
    """The core's register map for the bench agent and fbench."""
    core = core or P25Core()
    blocks = []
    for bank_offset, bank in sorted(core.register_map.registers.items()):
        # The control bank is in the AXI-Lite domain; the others cross into `sync`.
        domain = 'axi_lite' if bank_offset == 0 else 'sync'
        regs = [_register(r.name, bank_offset + word * bank.nstrobes, r.fields, domain,
                          f'0x{PRODUCT_ID:08X}' if r.name == 'product_id' else None)
                for word, r in sorted(bank.registers.items())]
        blocks.append({'name': bank.name, 'offset': f'0x{bank_offset:03X}', 'domain': domain,
                       'regs': regs})
    window = 4 << ADDRESS_WIDTH
    return {
        'schema': 'fbench.regmap/1',
        'core': 'p25',
        'base': f'0x{BASE:08X}',
        'size': window,
        'decode_limit': f'0x{window:03X}',
        'id_reg': 'product_id',
        'id_value': f'0x{PRODUCT_ID:08X}',
        'version': core.register_map.meta['version'],
        'snapshot_domains': {},
        'requires': {'uio': 'p25-core'},
        # The bridge answers the sync-domain banks while `sdr_reset` holds them, but with zeros
        # and dropped writes: the agent keeps away until the reset clears.
        'reset_gate': {
            'reg': 'control', 'bit': 0, 'domains': ['sync'],
            'desc': 'control.sdr_reset (powers up 1) holds the sync domain in reset; its banks '
                    'read 0 and drop writes until it clears',
        },
        'source': 'generated from scanner-hdl/radio_core by radio_core.bench_map',
        'blocks': blocks,
    }


def write_bench_map(path):
    with open(path, 'w', encoding='utf-8', newline='\n') as f:
        f.write(json.dumps(bench_map(), indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description="Write the radio core's bench register map")
    parser.add_argument('output', nargs='?',
                        default=os.path.join('..', 'bench', 'share', 'p25_regs.json'))
    args = parser.parse_args()
    write_bench_map(args.output)
    print(f'bench map written to {args.output}')


if __name__ == '__main__':
    main()
