"""Generate scanner/src/audio/codec/imbe/ambe/tables.rs from jmbe v1.0.9 Java enums.

Literals are copied as text so the Rust values parse to the same f32/f64.
"""
import re
import sys
from pathlib import Path

SRC = Path(sys.argv[1])  # .../codec/src/main/java/jmbe/codec/ambe
OUT = Path(sys.argv[2])


def entries(name, prefix):
    text = (SRC / name).read_text()
    rows = re.findall(r"^\s+(%s\w*)\((.*)\)[,;]\s*$" % prefix, text, re.M)
    return rows


def check_sequential(rows, prefix):
    for i, (n, _) in enumerate(rows):
        assert n == f"{prefix}{i}", (n, i)


def floats(s):
    return [x.strip() for x in re.findall(r"-?\d+\.\d+f?", s)]


def f32(lit):
    lit = lit.rstrip("f")
    return lit


out = []
w = out.append
w("//! AMBE 3600x2450 codebooks, generated from jmbe v1.0.9's enums")
w("//! (`codec/ambe/*.java`) with `reference/gen_tables.py`. Index = enum ordinal.")
w("")
w("#![allow(clippy::excessive_precision, clippy::approx_constant)]")
w("")

# Fundamental frequency: (index, frequency(double expr), L, FrameType)
text = (SRC / "AMBEFundamentalFrequency.java").read_text()
rows = re.findall(r"^\s+W(\d+)\((\d+),\s*([^,]+),\s*(\d+),\s*FrameType\.(\w+)\)", text, re.M)
assert len(rows) == 128, len(rows)
w("/// `AMBEFundamentalFrequency`: frequency (cycles per sample, as the Java")
w("/// double literal), harmonic count L and frame type, indexed by b0.")
w("pub(super) const FUNDAMENTAL: [(f64, usize, FrameType); 128] = [")
for i, (_w, idx, freq, l, ft) in enumerate(rows):
    assert int(idx) == i and int(_w) == i
    freq = freq.strip()
    if freq == "Math.PI / 32.0":
        freq = "core::f64::consts::PI / 32.0"
    elif freq == "0":
        freq = "0.0"
    ft = {"VOICE": "Voice", "ERASURE": "Erasure", "SILENCE": "Silence", "TONE": "Tone"}[ft]
    w(f"    ({freq}, {l}, FrameType::{ft}),")
w("];")
w("")

# Voicing decisions
rows = entries("AMBEVoicingDecision.java", "V")
check_sequential(rows, "V")
assert len(rows) == 32
w("/// `AMBEVoicingDecision`: voiced flag for each of 8 bands, indexed by b1.")
w("pub(super) const VOICING: [[bool; 8]; 32] = [")
for n, args in rows:
    vals = re.findall(r"true|false", args)
    assert len(vals) == 8
    w(f"    [{', '.join(vals)}],")
w("];")
w("")

# Differential gain
rows = entries("DifferentialGain.java", "G")
check_sequential(rows, "G")
assert len(rows) == 32
w("/// `DifferentialGain`: (gain, adjustment), indexed by b2. jmbe adds them")
w("/// in f32 (`getGain()`).")
w("pub(super) const DIFFERENTIAL_GAIN: [(f32, f32); 32] = [")
for n, args in rows:
    v = floats(args)
    assert len(v) == 2
    w(f"    ({f32(v[0])}, {f32(v[1])}),")
w("];")
w("")

# PRBA24
rows = entries("PRBA24.java", "V")
check_sequential(rows, "V")
assert len(rows) == 512
w("/// `PRBA24`: G2, G3, G4, indexed by b3.")
w("pub(super) const PRBA24: [[f32; 3]; 512] = [")
for n, args in rows:
    v = floats(args)
    assert len(v) == 3
    w(f"    [{', '.join(f32(x) for x in v)}],")
w("];")
w("")

# PRBA58
rows = entries("PRBA58.java", "V")
check_sequential(rows, "V")
assert len(rows) == 128
w("/// `PRBA58`: G5, G6, G7, G8, indexed by b4.")
w("pub(super) const PRBA58: [[f32; 4]; 128] = [")
for n, args in rows:
    v = floats(args)
    assert len(v) == 4
    w(f"    [{', '.join(f32(x) for x in v)}],")
w("];")
w("")

for name, size, b in (("HOCB5", 32, "b5"), ("HOCB6", 16, "b6"), ("HOCB7", 16, "b7"), ("HOCB8", 8, "b8")):
    rows = entries(f"{name}.java", "V")
    check_sequential(rows, "V")
    assert len(rows) == size, (name, len(rows))
    w(f"/// `{name}`: higher order coefficients, indexed by {b}.")
    w(f"pub(super) const {name}: [[f32; 4]; {size}] = [")
    for n, args in rows:
        v = floats(args)
        assert len(v) == 4, (name, n, args)
        w(f"    [{', '.join(f32(x) for x in v)}],")
    w("];")
    w("")

# LMPR block lengths
rows = entries("LMPRBlockLength.java", "L")
check_sequential(rows, "L")
assert len(rows) == 57
w("/// `LMPRBlockLength`: block lengths J[1..=4] (J[0] unused), indexed by L.")
w("pub(super) const LMPR_BLOCK_LENGTH: [[usize; 5]; 57] = [")
for n, args in rows:
    v = re.findall(r"\d+", args)
    assert len(v) == 5
    w(f"    [{', '.join(v)}],")
w("];")
w("")

# Tones
text = (SRC / "Tone.java").read_text()
rows = re.findall(r'^\s+(T\w+)\((\d+),\s*"([^"]*)",\s*([^,]+),\s*([^)]+)\)', text, re.M)
w("/// `Tone`: (value, label, frequency 1, frequency 2) in Hz; frequency 2 is")
w("/// 0.0 for single tones. Values not listed are `Tone.INVALID`.")
w("pub(super) const TONES: [(u8, &str, f64, f64); %d] = [" % len(rows))
for n, val, label, f1, f2 in rows:
    w(f'    ({val}, "{label}", {f1.strip()}, {f2.strip()}),')
w("];")
w("")
w("use super::FrameType;")

OUT.write_text("\n".join(out) + "\n", newline="\n")
print("wrote", OUT, "tones", len(rows))
