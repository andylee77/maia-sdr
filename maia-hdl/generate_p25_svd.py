#!/usr/bin/env python3
"""Generate SVD file from P25 core register map.

Usage:
    PYTHONPATH=. python3 generate_p25_svd.py [output_path]

Default output: ../p25-httpd/p25-pac/p25.svd
"""

import sys
import os

from p25_hdl.p25_top import write_svd

if __name__ == '__main__':
    output = sys.argv[1] if len(sys.argv) > 1 else \
        os.path.join('..', 'p25-httpd', 'p25-pac', 'p25.svd')
    os.makedirs(os.path.dirname(output), exist_ok=True)
    write_svd(output)
    print(f'SVD written to {output}')
