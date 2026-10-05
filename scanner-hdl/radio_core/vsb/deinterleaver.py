#
# Fishball radio core - the 8-VSB deinterleaver and Reed-Solomon's syndromes (pl/decoder.rs
# `Deinterleaver`, `Decoder::codeword_byte`).
#
# The field's bytes come in order. Branch j = t mod 52 of the commutator holds a FIFO of
# 4 (51 - j) bytes: a byte goes in and the branch's oldest comes out, so every byte leaves
# 52 x 4 x 51 bytes after it went in. The FIFOs share one block RAM (5,304 bytes), each branch's
# place in it kept in LUT RAM; the first 10,608 bytes out are the FIFOs' fill and are dropped.
#
# The bytes out are codewords of 207. Each codeword's 20 syndromes are Horner sums over GF(256)
# (multiplications by constants: XOR networks), and its flag says one is not zero. The codewords
# are counted 312 to a field.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *
from amaranth.lib.memory import Memory

from .tables import (BRANCHES, DATA_SEGMENTS, DEINTERLEAVER_DELAY, RS_N, RS_PARITY,
                     deinterleaver_bases, gf_times_matrix)


def _gf_times(m, value, j, name):
    """value x alpha^j over GF(256), as XORs of value's bits."""
    out = Signal(8, name=name)
    masks = gf_times_matrix(j)
    bits = []
    for mask in masks:
        terms = [value[i] for i in range(8) if (mask >> i) & 1]
        x = C(0, 1)
        for t in terms:
            x = x ^ t
        bits.append(x)
    m.d.comb += out.eq(Cat(*bits))
    return out


class Deinterleaver(Elaboratable):
    """The deinterleaver, the syndromes and the codewords' count.

    Attributes
    ----------
    restart : in
        Starts a run: the next byte in is a field's first.
    in_valid, in_byte : in
        The field's bytes in order.
    out_valid, out_byte, out_place : out
        A codeword's byte and its place (0-206).
    out_codeword, out_field : out
        The codeword's place in its field (0-311) and its field in the run.
    out_end, out_flag : out
        With the codeword's last byte: a syndrome is not zero.
    """
    def __init__(self):
        self.restart = Signal()
        self.in_valid = Signal()
        self.in_byte = Signal(8)
        self.out_valid = Signal()
        self.out_byte = Signal(8)
        self.out_place = Signal(range(RS_N))
        self.out_codeword = Signal(range(DATA_SEGMENTS))
        self.out_field = Signal(32)
        self.out_end = Signal()
        self.out_flag = Signal()

    def elaborate(self, platform):
        m = Module()
        bases, lengths = deinterleaver_bases()

        # ── The commutator and each branch's place ───────────────────────────────────────
        branch = Signal(range(BRANCHES))
        filled = Signal(range(DEINTERLEAVER_DELAY + 1))
        m.submodules.places = places = Memory(shape=8, depth=64, init=[])
        prd = places.read_port(domain='comb')
        pwr = places.write_port()
        fresh = Signal(BRANCHES, init=(1 << BRANCHES) - 1)
        base = Array(C(b, 13) for b in bases)[branch]
        length = Array(C(n, 8) for n in lengths)[branch]
        ptr = Signal(8)
        m.d.comb += [prd.addr.eq(branch), ptr.eq(Mux(fresh.bit_select(branch, 1), 0, prd.data))]
        m.d.comb += [pwr.en.eq(self.in_valid), pwr.addr.eq(branch),
                     pwr.data.eq(Mux(ptr + 1 >= length, 0, ptr + 1))]

        m.submodules.fifo = fifo = Memory(shape=8, depth=sum(lengths), init=[])
        frd = fifo.read_port(transparent_for=())
        fwr = fifo.write_port()
        a_valid = Signal()
        a_byte = Signal(8)
        a_addr = Signal(13)
        a_through = Signal()
        a_out = Signal()
        m.d.sync += a_valid.eq(self.in_valid)
        with m.If(self.in_valid):
            m.d.sync += [
                a_byte.eq(self.in_byte), a_addr.eq(base + ptr),
                a_through.eq(branch == BRANCHES - 1),
                a_out.eq(filled == DEINTERLEAVER_DELAY),
                branch.eq(Mux(branch == BRANCHES - 1, 0, branch + 1)),
                fresh.bit_select(branch, 1).eq(0),
            ]
            with m.If(filled < DEINTERLEAVER_DELAY):
                m.d.sync += filled.eq(filled + 1)
        with m.If(self.restart):
            m.d.sync += [branch.eq(0), filled.eq(0), fresh.eq((1 << BRANCHES) - 1)]
        # The branch's oldest byte read and the new one written in its place.
        m.d.comb += [frd.addr.eq(a_addr), frd.en.eq(1),
                     fwr.en.eq(a_valid & ~a_through), fwr.addr.eq(a_addr), fwr.data.eq(a_byte)]
        b_valid = Signal()
        b_through = Signal()
        b_byte = Signal(8)
        m.d.sync += [b_valid.eq(a_valid & a_out), b_through.eq(a_through), b_byte.eq(a_byte)]
        byte = Signal(8)
        m.d.comb += byte.eq(Mux(b_through, b_byte, frd.data))

        # ── Codewords and their syndromes ────────────────────────────────────────────────
        place = Signal(range(RS_N))
        codeword = Signal(range(DATA_SEGMENTS))
        field = Signal(32)
        syn = [Signal(8, name=f'syn{i}') for i in range(RS_PARITY)]
        new = []
        for i in range(RS_PARITY):
            times = _gf_times(m, syn[i], i, f'syn_times{i}')
            v = Signal(8, name=f'syn_new{i}')
            m.d.comb += v.eq(Mux(place == 0, 0, times) ^ byte)
            new.append(v)
        last = place == RS_N - 1
        m.d.sync += [self.out_valid.eq(b_valid), self.out_end.eq(b_valid & last)]
        with m.If(b_valid):
            m.d.sync += [
                *[syn[i].eq(new[i]) for i in range(RS_PARITY)],
                self.out_byte.eq(byte), self.out_place.eq(place),
                self.out_codeword.eq(codeword), self.out_field.eq(field),
                self.out_flag.eq(Cat(*new).any()),
                place.eq(Mux(last, 0, place + 1)),
            ]
            with m.If(last):
                m.d.sync += codeword.eq(Mux(codeword == DATA_SEGMENTS - 1, 0, codeword + 1))
                with m.If(codeword == DATA_SEGMENTS - 1):
                    m.d.sync += field.eq(field + 1)
        with m.If(self.restart):
            m.d.sync += [place.eq(0), codeword.eq(0), field.eq(0)]
        return m
