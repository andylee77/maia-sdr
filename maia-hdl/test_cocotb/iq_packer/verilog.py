#!/usr/bin/env python3
#
# Fishball P25 - IQPacker cocotb verilog generator (Phase 6C)
#
# SPDX-License-Identifier: MIT
#

from amaranth.back.verilog import convert

from p25_hdl.iq_packer import IQPacker


def main():
    with open('dut.v', 'w') as f:
        m = IQPacker()
        f.write(convert(
            m, name='dut',
            ports=[m.re_in, m.im_in, m.strobe_in, m.stream_ready,
                   m.data_out, m.data_valid, m.overflow],
            emit_src=False))


if __name__ == '__main__':
    main()
