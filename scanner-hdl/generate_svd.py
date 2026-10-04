#!/usr/bin/env python3
"""Generate the SVD file of the core's register map.

Usage, from scanner-hdl/ (radio_core imports upstream maia_hdl from ../maia-hdl):
    PYTHONPATH=.:../maia-hdl python3 generate_svd.py [output_path]

Default output: ../scanner/core-pac/core.svd (then, in scanner/core-pac:
svd2rust -i core.svd --target none && mv lib.rs src/lib.rs)
"""

import sys
import os

from radio_core.p25_top import write_svd

if __name__ == '__main__':
    output = sys.argv[1] if len(sys.argv) > 1 else \
        os.path.join('..', 'scanner', 'core-pac', 'core.svd')
    os.makedirs(os.path.dirname(output), exist_ok=True)
    write_svd(output)
    print(f'SVD written to {output}')
