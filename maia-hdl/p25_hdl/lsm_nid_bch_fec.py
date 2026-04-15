#
# Fishball P25 -- LSM NID BCH(63,16,11) FEC decoder
#
# Phase 6E.7 of the LSM HDL port. Hardware port of the
# maximum-likelihood NID FEC decoder in
# `p25-httpd/src/lsm/nid_fec.rs` (which is itself a port of
# `tools/p25_nid_fec.py`, validated against SDRTrunk's
# `BCH_63_16_23_P25_Test.java` encoder).
#
# Why "compute on the fly" instead of a BRAM codebook
# ----------------------------------------------------
# The Phase 6E dev plan originally called for a 65536-entry codebook
# in BRAM (4.2 Mbit) plus a popcount tree. That works in software --
# `nid_fec.rs` keeps a `[u64; 65536]` static codebook and sweeps it
# linearly. But on Z7020 4.2 Mbit is ~84 % of all available BRAM,
# leaving very little headroom for the existing C4FM chain, the
# Phase 6E LSM front end, and any future expansion.
#
# Crucially, the codebook does not need to be *stored*. Each
# 64-bit codeword is derived from its 16-bit data word by XOR'ing
# at most 16 constant 48-bit generator rows -- pure combinational
# logic. So we sweep a 16-bit counter `data` 0..65535, build the
# corresponding codeword combinationally on each cycle, XOR with the
# received NID, popcount the difference, and keep a running min.
# Same algorithm, same correction strength, same cycle budget --
# zero BRAM.
#
# This sub-phase deviates from the original plan with the user's
# explicit approval. See `doc/changes/016_phase6e7_bch_fec.md`
# for the full architecture comparison.
#
# Algorithm
# ---------
# Per cycle in the SWEEPING state:
#
#     data        = sweep_counter[0:16]                 # 16-bit
#     parity      = XOR over { GEN[i] : data[15-i]==1 } # 48-bit
#     codeword    = (data << 48) | parity               # 64-bit
#     diff        = codeword ^ received_nid_latched     # 64-bit
#     dist        = popcount(diff)                      # 7-bit
#     if dist < best_dist:
#         best_dist <= dist
#         best_data <= data
#
# After 65536 cycles the loop has compared the received word
# against every valid codeword. If `best_dist <= 11` the decode is
# inside the BCH(63,16,d=23) unique-decoding sphere and the result
# is bit-exact with what the Rust ML decoder produces. Beyond 11
# bit errors the result may be a different valid codeword (also
# what the Rust decoder reports), but `valid_out=0` flags it as
# uncorrectable to the caller.
#
# Bit ordering -- matches `tools/p25_nid_fec.py:encode_nid()`:
#
#     bit 63 .. 48 : data word (NAC[11..0] || DUID[3..0],
#                                MSB-first within the 16-bit field)
#     bit 47 ..  0 : 48 BCH parity bits
#
# Cycle / latency budget
# ----------------------
# - 65536 sweep cycles + 1 setup + 1 finish = ~65538 cycles per
#   decode. At 100 MHz that is ~656 us.
# - The NID FEC budget is one decode per NID, NIDs are spaced at
#   ~14 ms minimum (one P25 frame), so the duty cycle is well under
#   5 % and sequential operation is fine. No pipelining required.
#
# Resource estimate (Z7020)
# -------------------------
# - Combinational parity: 16 * 48 = 768 bits of constant XOR. Each
#   of the 48 parity bits is the XOR of ~8 data bits on average
#   (sparse generator) -- ~2 LUT levels.
# - 64-bit popcount tree: ~6 LUT/adder levels.
# - 7-bit min comparator + 16-bit best_data register.
# - One 16-bit (or 17-bit, see below) sweep counter.
# - Total: <1 % of Z7020 LUT/FF, ZERO BRAM, ZERO DSP48E1.
#
# This is a pure-LUT implementation. The 13-ish levels of
# combinational logic (parity XOR + 64-bit XOR + popcount tree +
# compare) is well within Vivado's reach at 100 MHz on Z7020
# (typical max ~15 LUT levels per period). If timing closure ever
# becomes a problem the natural pipelining cut is between popcount
# and the comparator -- a single registered stage doubles the
# latency but adds zero functional complexity.
#
# I/O
# ---
# Inputs (sync domain):
#     start            : Signal()         -- pulse to begin a decode
#     received_nid     : Signal(64)       -- latched on `start`
#
# Outputs (sync domain):
#     done             : Signal()         -- 1 cycle pulse when
#                                            decode is complete
#     nac_out          : Signal(12)
#     duid_out         : Signal(4)
#     n_errors_out     : Signal(7)        -- 0..63 (Hamming dist
#                                            to nearest codeword)
#     valid_out        : Signal()         -- 1 if n_errors_out <= 11
#                                            (inside the sphere)
#     busy             : Signal()         -- high during a decode
#
# A `start` pulse while `busy` is high is ignored (the in-flight
# decode runs to completion). The caller should wait for `done`
# before issuing the next decode.
#
# SPDX-License-Identifier: MIT
#

from amaranth import *


# ----------------------------------------------------------------
# Constants -- mirror `nid_fec.rs` and `tools/p25_nid_fec.py`
# ----------------------------------------------------------------

NAC_BITS = 12
DUID_BITS = 4
DATA_BITS = NAC_BITS + DUID_BITS    # 16
PARITY_BITS = 48
CODE_BITS = DATA_BITS + PARITY_BITS  # 64
N_CODEWORDS = 1 << DATA_BITS         # 65536
T_MAX_ERRORS = 11

# 16-row systematic generator matrix from
# `BCH_63_16_23_P25_Test.java` -- octal literals copied verbatim
# via `tools/p25_nid_fec.py` and `nid_fec.rs`. Each row is the
# 48-bit parity contribution selected by data bit `x` (MSB-first).
P25_NID_GENERATOR_MATRIX = [
    0o6331141367235452,  # row  0
    0o5265521614723276,  # row  1
    0o4603711461164164,  # row  2
    0o2301744630472072,  # row  3
    0o7271623073000466,  # row  4
    0o5605650752635660,  # row  5
    0o2702724365316730,  # row  6
    0o1341352172547354,  # row  7
    0o0560565075263566,  # row  8
    0o6141333751704220,  # row  9
    0o3060555764742110,  # row 10
    0o1430266772361044,  # row 11
    0o0614133375170422,  # row 12
    0o6037114611641642,  # row 13
    0o5326507063515373,  # row 14
    0o4662302756473127,  # row 15
]

# Sanity-check: row 0 fits in 48 bits.
for _i, _row in enumerate(P25_NID_GENERATOR_MATRIX):
    assert 0 <= _row < (1 << PARITY_BITS), \
        f"GEN[{_i}] = {_row:#x} does not fit in 48 bits"


def encode_nid(nac: int, duid: int) -> int:
    """Software reference encoder -- bit-exact with `nid_fec.rs`.

    Used by the test bench to build golden codewords. NOT used by
    the HDL itself (the HDL recomputes parity combinationally).
    """
    if not (0 <= nac < (1 << NAC_BITS)):
        raise ValueError(f"NAC out of range: {nac}")
    if not (0 <= duid < (1 << DUID_BITS)):
        raise ValueError(f"DUID out of range: {duid}")
    data_word = (nac << DUID_BITS) | duid
    parity = 0
    for bit_idx in range(DATA_BITS):
        if data_word & (1 << (DATA_BITS - 1 - bit_idx)):
            parity ^= P25_NID_GENERATOR_MATRIX[bit_idx]
    return (data_word << PARITY_BITS) | parity


class LsmNidBchFec(Elaboratable):
    """Compute-on-the-fly ML decoder for the P25 NID BCH(63,16,11).

    Sweeps all 65536 valid codewords combinationally, computes
    Hamming distance to the latched received word, and reports the
    nearest codeword. Within the BCH unique-decoding sphere
    (<=11 errors) this is bit-exact with `lsm::nid_fec::decode_nid`.

    See module-level docstring for the full architecture rationale.
    """

    def __init__(self):
        # ── Inputs ──────────────────────────────────────────────
        self.start = Signal()
        self.received_nid = Signal(CODE_BITS)
        # Phase 8A: runtime reset. A 1-cycle pulse aborts any
        # in-flight sweep (~65538 cycles = ~1 ms) and drops the
        # FSM back to IDLE.
        self.reset_in = Signal()

        # ── Outputs ─────────────────────────────────────────────
        self.done = Signal()
        self.nac_out = Signal(NAC_BITS, reset_less=True)
        self.duid_out = Signal(DUID_BITS, reset_less=True)
        # Hamming distance is at most 64 -> 7 bits.
        self.n_errors_out = Signal(7, reset_less=True)
        self.valid_out = Signal(reset_less=True)
        self.busy = Signal()

    def elaborate(self, platform):
        m = Module()

        # ── Sweep state ────────────────────────────────────────
        # 17-bit counter so bit 16 cleanly signals "swept all
        # 65536 data words". sweep_data is the low 16 bits, used
        # as the current data word being tested.
        counter = Signal(DATA_BITS + 1, init=0, reset_less=True)
        sweep_done = counter[DATA_BITS]
        sweep_data = counter[:DATA_BITS]

        # Latched copy of the input on `start`. The caller may
        # change `received_nid` during the sweep without affecting
        # the in-flight decode.
        received_q = Signal(CODE_BITS, reset_less=True)

        # Running minimum.
        # best_dist init = 0xFF (>> 64, sentinel). The first
        # comparison will always overwrite it.
        BEST_DIST_WIDTH = 7
        best_dist = Signal(BEST_DIST_WIDTH, init=0x7F, reset_less=True)
        best_data = Signal(DATA_BITS, init=0, reset_less=True)

        # ── State machine ──────────────────────────────────────
        with m.FSM(init="IDLE") as fsm:
            with m.State("IDLE"):
                m.d.sync += self.done.eq(0)
                with m.If(self.start):
                    m.d.sync += [
                        received_q.eq(self.received_nid),
                        counter.eq(0),
                        best_dist.eq(0x7F),
                        best_data.eq(0),
                    ]
                    m.next = "SWEEP"

            with m.State("SWEEP"):
                # Phase 8A: abort the in-flight sweep on runtime
                # reset. IDLE is already a self-loop so it does not
                # need this check.
                with m.If(self.reset_in):
                    m.next = "IDLE"
                # The combinational dist/update logic below uses
                # `sweep_data` and writes `best_dist`/`best_data`
                # synchronously. Counter advances every cycle until
                # it has processed data words 0..65535 (i.e. when
                # bit 16 is set, we are done sweeping).
                with m.If(sweep_done):
                    # Latch outputs and emit `done` next cycle.
                    m.d.sync += [
                        self.nac_out.eq(
                            best_data[DUID_BITS:DATA_BITS]),
                        self.duid_out.eq(best_data[:DUID_BITS]),
                        self.n_errors_out.eq(best_dist),
                        self.valid_out.eq(best_dist <= T_MAX_ERRORS),
                        self.done.eq(1),
                    ]
                    m.next = "IDLE"
                with m.Else():
                    m.d.sync += counter.eq(counter + 1)

        # `busy` reflects "decode in progress" -- anything other
        # than IDLE.
        m.d.comb += self.busy.eq(~fsm.ongoing("IDLE"))

        # ── Combinational codeword + distance for `sweep_data` ─
        # 1) Parity = XOR over { GEN[i] : sweep_data[15-i]==1 }.
        #    Build it as a per-bit XOR tree so synthesis can pack
        #    it into LUT6 fabric efficiently.
        parity = Signal(PARITY_BITS)
        parity_bit_exprs = []
        for bit in range(PARITY_BITS):
            # Which data bits contribute to parity[bit]?
            # GEN[i] contributes its bit `bit` if (GEN[i]>>bit)&1.
            terms = []
            for i in range(DATA_BITS):
                if (P25_NID_GENERATOR_MATRIX[i] >> bit) & 1:
                    # data MSB-first: bit `i` of the 16-bit data
                    # word in cbm-order is sweep_data[15-i] in
                    # numeric order.
                    terms.append(sweep_data[DATA_BITS - 1 - i])
            if not terms:
                expr = Const(0, 1)
            else:
                expr = terms[0]
                for t in terms[1:]:
                    expr = expr ^ t
            parity_bit_exprs.append(expr)
        m.d.comb += parity.eq(Cat(*parity_bit_exprs))

        # 2) Codeword = (data << 48) | parity.
        codeword = Signal(CODE_BITS)
        m.d.comb += codeword.eq(Cat(parity, sweep_data))

        # 3) diff = codeword ^ received_q
        diff = Signal(CODE_BITS)
        m.d.comb += diff.eq(codeword ^ received_q)

        # 4) dist = popcount(diff)
        # `sum(...)` of single-bit signals returns a multi-bit
        # arithmetic sum -- Amaranth lets the synthesis tool build
        # the adder tree.
        dist = Signal(BEST_DIST_WIDTH)
        m.d.comb += dist.eq(sum(diff[i] for i in range(CODE_BITS)))

        # 5) Update running min while we are actively sweeping
        # (FSM in SWEEP and counter has not yet rolled over).
        update_best = Signal()
        m.d.comb += update_best.eq(
            fsm.ongoing("SWEEP") & ~sweep_done)

        with m.If(update_best & (dist < best_dist)):
            m.d.sync += [
                best_dist.eq(dist),
                best_data.eq(sweep_data),
            ]

        # ── Phase 8A runtime reset override ─────────────────────
        # Abort any in-flight sweep, clear the running-min
        # registers, and reset the output latches so a restart
        # looks like cold-boot. The FSM itself is forced back to
        # IDLE by the `m.next = "IDLE"` in SWEEP above.
        with m.If(self.reset_in):
            m.d.sync += [
                counter.eq(0),
                received_q.eq(0),
                best_dist.eq(0x7F),
                best_data.eq(0),
                self.nac_out.eq(0),
                self.duid_out.eq(0),
                self.n_errors_out.eq(0),
                self.valid_out.eq(0),
                self.done.eq(0),
            ]

        return m
