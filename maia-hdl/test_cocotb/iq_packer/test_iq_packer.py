#
# Fishball P25 - IQPacker cocotb test (Phase 6C)
#
# Validates the post-DDC IQ packer that buffers two consecutive
# (re, im) sample pairs into a 64-bit AXI4-Stream word for the
# ring DMA. See p25_hdl/iq_packer.py for the bit layout.
#
# SPDX-License-Identifier: MIT
#

import cocotb
from cocotb.clock import Clock
from cocotb.triggers import RisingEdge, ClockCycles


def s16(x):
    """Two's complement masking to 16 bits, matching IQPacker.re_in/im_in."""
    return x & 0xFFFF


def expected_word(re0, im0, re1, im1):
    """Bit layout: {im1[15:0], re1[15:0], im0[15:0], re0[15:0]}.

    Sample 0 in low half, sample 1 in high half. See iq_packer.py
    docstring for the diagram.
    """
    return ((s16(im1) << 48)
            | (s16(re1) << 32)
            | (s16(im0) << 16)
            | s16(re0))


async def reset(dut):
    dut.rst.value = 1
    dut.re_in.value = 0
    dut.im_in.value = 0
    dut.strobe_in.value = 0
    dut.stream_ready.value = 1
    await ClockCycles(dut.clk, 4)
    dut.rst.value = 0
    await ClockCycles(dut.clk, 2)


async def strobe_pair(dut, re, im):
    """Drive one (re, im) pair on the next rising edge with strobe asserted."""
    await RisingEdge(dut.clk)
    dut.re_in.value = s16(re)
    dut.im_in.value = s16(im)
    dut.strobe_in.value = 1
    await RisingEdge(dut.clk)
    dut.strobe_in.value = 0


@cocotb.test()
async def test_packing(dut):
    """Drive a deterministic sequence and verify packed words exactly.

    Pattern: re = +1, +2, +3, ...; im = -1, -2, -3, ...
    Expected words after each pair of strobes:
      pair 0,1: {im1=-2, re1=+2, im0=-1, re0=+1}
      pair 2,3: {im1=-4, re1=+4, im0=-3, re0=+3}
      ...
    """
    cocotb.start_soon(Clock(dut.clk, 10, units='ns').start())
    await reset(dut)

    expected = []
    n_pairs = 8
    for k in range(n_pairs):
        re0, im0 = (2 * k + 1), -(2 * k + 1)
        re1, im1 = (2 * k + 2), -(2 * k + 2)
        expected.append(expected_word(re0, im0, re1, im1))

    captured = []
    for k in range(n_pairs):
        re0, im0 = (2 * k + 1), -(2 * k + 1)
        re1, im1 = (2 * k + 2), -(2 * k + 2)
        await strobe_pair(dut, re0, im0)
        # data_valid should NOT be high yet (only first half latched)
        await RisingEdge(dut.clk)
        assert dut.data_valid.value == 0, \
            f'pair {k}: data_valid asserted after first sample'
        await strobe_pair(dut, re1, im1)
        # After second strobe + 1 cycle, data_valid should be high
        await RisingEdge(dut.clk)
        assert dut.data_valid.value == 1, \
            f'pair {k}: data_valid not asserted after second sample'
        captured.append(int(dut.data_out.value))
        # Backpressure-free: stream_ready=1 means valid clears next cycle
        await RisingEdge(dut.clk)
        assert dut.data_valid.value == 0, \
            f'pair {k}: data_valid did not clear after handshake'

    assert captured == expected, (
        f'packed words mismatch:\n'
        f'  expected = {[hex(w) for w in expected]}\n'
        f'  captured = {[hex(w) for w in captured]}'
    )
    assert dut.overflow.value == 0, 'overflow should be clear in happy path'


@cocotb.test()
async def test_overflow(dut):
    """Hold stream_ready=0 across two complete pairs and verify overflow latches."""
    cocotb.start_soon(Clock(dut.clk, 10, units='ns').start())
    await reset(dut)

    # Block backpressure forever
    dut.stream_ready.value = 0

    # First complete pair: latches data_valid, no overflow yet
    await strobe_pair(dut, 1, -1)
    await strobe_pair(dut, 2, -2)
    await ClockCycles(dut.clk, 2)
    assert dut.data_valid.value == 1, \
        'data_valid should be high after first pair'
    assert dut.overflow.value == 0, \
        'overflow should still be clear after one stalled pair'

    # Second complete pair while first is still stalled: must latch overflow
    await strobe_pair(dut, 3, -3)
    await strobe_pair(dut, 4, -4)
    await ClockCycles(dut.clk, 2)
    assert dut.overflow.value == 1, \
        'overflow should latch when a new word arrives while previous is stalled'

    # Overflow is sticky in gateware (clears only via Rsticky AXI read in real
    # use). Confirm it stays set after stream_ready goes high again.
    dut.stream_ready.value = 1
    await ClockCycles(dut.clk, 4)
    assert dut.overflow.value == 1, \
        'overflow should stay sticky in gateware (cleared only by AXI Rsticky)'
