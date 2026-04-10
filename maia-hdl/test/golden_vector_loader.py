#
# Phase 6E.0 -- Loader for the LSM HDL port golden vectors.
#
# Reads JSON files from `maia-hdl/test/golden_vectors/`, produced by the
# Rust test `lsm::golden_dump::*` in `p25-httpd/src/lsm/golden_dump.rs`.
# Each downstream amaranth-sim test in `test_lsm_*.py` calls
# `load_iq_stage()` or `load_demod_stage()` here to get a fixed,
# regen-stable reference for the HDL block under test.
#
# Format of the JSON files is documented at the top of
# `p25-httpd/src/lsm/golden_dump.rs`. This loader is intentionally
# kept tiny -- the tests do their own fixed-point conversion and
# tolerance handling.
#
# To regenerate the JSON files after a change in the Rust pipeline:
#
#     cd p25-httpd
#     cargo test --bin p25-httpd lsm::golden_dump
#
# SPDX-License-Identifier: MIT
#

import json
import os
from dataclasses import dataclass


# Repo-relative path to the golden vector directory.
GOLDEN_VECTORS_DIR = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    'golden_vectors',
)


@dataclass
class IQStage:
    """One JSON file from the IQ -> IQ stage emitter."""
    name: str
    stage: str
    input_rate_hz: float
    output_rate_hz: float
    input_re: list
    input_im: list
    output_re: list
    output_im: list

    @property
    def n_input(self):
        return len(self.input_re)

    @property
    def n_output(self):
        return len(self.output_re)


@dataclass
class DemodStage:
    """The `demod_loop_synthetic` JSON file (richer per-symbol output)."""
    name: str
    stage: str
    input_rate_hz: float
    symbol_rate_hz: float
    samples_per_symbol: float
    input_re: list
    input_im: list
    soft_re: list
    soft_im: list
    soft_phase: list
    hard_dibit: list
    pll: list
    sample_point: list
    truth_dibit: list

    @property
    def n_input(self):
        return len(self.input_re)

    @property
    def n_symbols(self):
        return len(self.hard_dibit)


def _load_json(name):
    path = os.path.join(GOLDEN_VECTORS_DIR, f'{name}.json')
    if not os.path.exists(path):
        raise FileNotFoundError(
            f"golden vector {name!r} not found at {path}; regenerate via "
            f"`cargo test --bin p25-httpd lsm::golden_dump` in p25-httpd/")
    with open(path, 'r') as f:
        return json.load(f)


def load_iq_stage(name):
    """Load an IQ -> IQ golden vector (decimator, LPF, RRC).

    Returns an :class:`IQStage` instance.
    """
    j = _load_json(name)
    return IQStage(
        name=j['name'],
        stage=j['stage'],
        input_rate_hz=float(j['input_rate_hz']),
        output_rate_hz=float(j['output_rate_hz']),
        input_re=j['input_re'],
        input_im=j['input_im'],
        output_re=j['output_re'],
        output_im=j['output_im'],
    )


def load_demod_stage(name='demod_loop_synthetic'):
    """Load the demod-loop golden vector with per-symbol traces.

    Returns a :class:`DemodStage` instance.
    """
    j = _load_json(name)
    return DemodStage(
        name=j['name'],
        stage=j['stage'],
        input_rate_hz=float(j['input_rate_hz']),
        symbol_rate_hz=float(j['symbol_rate_hz']),
        samples_per_symbol=float(j['samples_per_symbol']),
        input_re=j['input_re'],
        input_im=j['input_im'],
        soft_re=j['soft_re'],
        soft_im=j['soft_im'],
        soft_phase=j['soft_phase'],
        hard_dibit=j['hard_dibit'],
        pll=j['pll'],
        sample_point=j['sample_point'],
        truth_dibit=j['truth_dibit'],
    )


def to_fixed(values, frac_bits, width=None, saturate=True):
    """Quantise a list of floats to signed integers in Q0.frac_bits.

    Used by the HDL tests to drive Amaranth signals from the f32
    reference. ``width`` (if given) clamps to a signed N-bit range.

    With ``saturate=True`` (default), values outside the
    representable range are clipped to the nearest endpoint -- the
    standard fixed-point convention, and the right behaviour for IQ
    streams nominally bounded by [-1, 1] in floating point that
    occasionally hit exactly 1.0 (which is one ULP outside Q15).
    With ``saturate=False`` an out-of-range value raises
    ``OverflowError``, useful when a test wants to assert all
    goldens fit cleanly in the chosen format.
    """
    scale = 1 << frac_bits
    out = []
    if width is not None:
        lo = -(1 << (width - 1))
        hi = (1 << (width - 1)) - 1
    for v in values:
        q = int(round(float(v) * scale))
        if width is not None and (q < lo or q > hi):
            if saturate:
                q = max(lo, min(hi, q))
            else:
                raise OverflowError(
                    f"value {v} -> Q0.{frac_bits} = {q} overflows "
                    f"signed {width}-bit range [{lo}, {hi}]")
        out.append(q)
    return out


if __name__ == '__main__':
    # Smoke test: load every fixture and print a one-line summary.
    for name in [
            'decimator_62k5_to_31k25',
            'lpf_31250',
            'rrc_31250',
    ]:
        s = load_iq_stage(name)
        print(f"{name:32s}  in={s.n_input:5d}  out={s.n_output:5d}  "
              f"rate {s.input_rate_hz:.0f} -> {s.output_rate_hz:.0f}")
    d = load_demod_stage()
    print(f"{d.name:32s}  in={d.n_input:5d}  symbols={d.n_symbols:5d}  "
          f"sps={d.samples_per_symbol:.4f}")
