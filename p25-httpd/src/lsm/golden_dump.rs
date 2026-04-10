//! Phase 6E.0 -- Golden vector emitter for the LSM HDL port.
//!
//! Drives the validated `crate::lsm` Rust pipeline (Phase 6D) on
//! deterministic synthetic inputs and dumps per-stage outputs to
//! `maia-hdl/test/golden_vectors/*.json`. Each downstream amaranth-sim
//! test in `maia-hdl/test/test_lsm_*.py` then loads its corresponding
//! JSON file and asserts bit-equivalence (within a fixed-point
//! tolerance) against the HDL output.
//!
//! Lives as a `#[cfg(test)]` module inside `lsm/` instead of in
//! `tests/` because `p25-httpd` is binary-only -- there is no
//! `lib.rs` for an integration test to import from. Putting it here
//! also keeps the producer next to the consumer (the actual LSM
//! pipeline modules) so a `git grep LPF_TAPS_31250` finds both ends.
//!
//! Run with:
//! ```text
//!   cd p25-httpd
//!   cargo test lsm::golden_dump:: -- --nocapture
//! ```
//!
//! Each test always (re)writes its golden file. The files are checked
//! into git so the amaranth-side HDL tests can run standalone without
//! a Rust toolchain.
//!
//! Output format (one file per stage), e.g. `lpf_31250.json`:
//!
//! ```json
//! {
//!   "name": "lpf_31250",
//!   "stage": "lsm::filters::StreamingFir(LPF_TAPS_31250)",
//!   "input_rate_hz": 31250.0,
//!   "output_rate_hz": 31250.0,
//!   "n_input": 2048,
//!   "n_output": 2048,
//!   "input_re":  [...f32 in scientific notation, length n_input...],
//!   "input_im":  [...],
//!   "output_re": [...length n_output...],
//!   "output_im": [...]
//! }
//! ```
//!
//! See `doc/changes/015_phase6e_lsm_hdl_port.md` for the porting log.

#![cfg(test)]

use std::f32::consts::PI;
use std::fs;
use std::path::PathBuf;

use super::demod::{demod_lsm_with_state, DemodState, P25_SYMBOL_RATE};
use super::filters::{
    apply_real_fir_complex, decimate_by_2, StreamingDecimator2, StreamingFir,
    LPF_TAPS_31250, POST_DECIMATION_RATE_HZ, RRC_TAPS_31250,
};
use super::Complex32;

// ─── JSON serialisation helpers ─────────────────────────────────────

/// Resolve `<repo>/maia-hdl/test/golden_vectors/<name>.json` from
/// CARGO_MANIFEST_DIR. Creates the directory if it does not exist.
fn golden_path(name: &str) -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest
        .parent()
        .expect("manifest dir has parent")
        .join("maia-hdl")
        .join("test")
        .join("golden_vectors");
    fs::create_dir_all(&dir).expect("create golden_vectors dir");
    dir.join(format!("{name}.json"))
}

/// Write a JSON file describing one stage's input and output IQ buffers.
///
/// Hand-rolled JSON instead of `serde_json::to_string_pretty` so we
/// control the f32 formatting. `{:.9e}` round-trips an f32 exactly
/// through decimal, which keeps the goldens regen-stable across
/// machines without per-line FP fuzzing on the consumer side.
fn write_stage_iq(
    name: &str,
    stage: &str,
    input_rate_hz: f32,
    output_rate_hz: f32,
    input: &[Complex32],
    output: &[Complex32],
) {
    let path = golden_path(name);
    let mut s = String::with_capacity(64 * (input.len() + output.len()));
    s.push_str("{\n");
    s.push_str(&format!("  \"name\": \"{name}\",\n"));
    s.push_str(&format!("  \"stage\": \"{stage}\",\n"));
    s.push_str(&format!("  \"input_rate_hz\": {input_rate_hz},\n"));
    s.push_str(&format!("  \"output_rate_hz\": {output_rate_hz},\n"));
    s.push_str(&format!("  \"n_input\": {},\n", input.len()));
    s.push_str(&format!("  \"n_output\": {},\n", output.len()));
    push_f32_array(&mut s, "input_re", input.iter().map(|c| c.re));
    s.push_str(",\n");
    push_f32_array(&mut s, "input_im", input.iter().map(|c| c.im));
    s.push_str(",\n");
    push_f32_array(&mut s, "output_re", output.iter().map(|c| c.re));
    s.push_str(",\n");
    push_f32_array(&mut s, "output_im", output.iter().map(|c| c.im));
    s.push_str("\n}\n");
    fs::write(&path, s).expect("write golden file");
    eprintln!("[golden] wrote {}", path.display());
}

fn push_f32_array<I: IntoIterator<Item = f32>>(s: &mut String, key: &str, vals: I) {
    s.push_str(&format!("  \"{key}\": ["));
    let mut first = true;
    for v in vals {
        if !first {
            s.push_str(", ");
        }
        first = false;
        s.push_str(&format!("{v:.9e}"));
    }
    s.push(']');
}

// ─── Synthetic input generators ─────────────────────────────────────

/// Deterministic linear-frequency-sweep complex input. Used as the
/// LPF/RRC test fixture: the sweep covers DC..Nyquist so the filter's
/// passband and stopband are both exercised in one buffer.
fn frequency_sweep(n: usize, sample_rate_hz: f32) -> Vec<Complex32> {
    // Phase φ(t) = 2π · 0.5·k·t² with k chosen so the instantaneous
    // frequency reaches Nyquist by the end of the buffer.
    let dt = 1.0 / sample_rate_hz;
    let total_t = n as f32 * dt;
    let f1 = sample_rate_hz / 2.0; // sweep up to Nyquist
    let k = f1 / total_t;
    (0..n)
        .map(|i| {
            let t = i as f32 * dt;
            let phi = 2.0 * PI * (0.5 * k * t * t);
            Complex32::new(phi.cos(), phi.sin())
        })
        .collect()
}

/// Build a synthetic P25 LSM-shaped baseband signal at `output_rate_hz`.
///
/// Steps (rough inverse of the receiver pipeline):
///   1. Take a known dibit sequence as ground truth.
///   2. Map each dibit to its ideal LSM phase delta (Dibit.java angles
///      ±π/4, ±3π/4 — same constants as `lsm::demod::dibit_phase`).
///   3. Cumulative-sum to absolute phase per symbol.
///   4. Linear-interpolate phase to `output_rate_hz / 4800` samples
///      per symbol, then take e^(j·phase).
///
/// Crude pulse shaping (linear interp instead of an actual transmit
/// RRC) — the receiver's RRC matched filter does the heavy lifting at
/// test time, and the goal here is just a continuous IQ stream that
/// gives the AGC, PLL and Gardner timing loops something to lock onto.
/// Truth comparison happens at the dibit layer, not in the IQ domain.
fn synth_lsm_iq(dibits: &[u8], output_rate_hz: f32) -> Vec<Complex32> {
    let sps = output_rate_hz / P25_SYMBOL_RATE; // ~6.51 at 31.25 kSPS
    let n_symbols = dibits.len();
    let total = (n_symbols as f32 * sps).floor() as usize;

    // Per-symbol absolute phase (cumulative).
    let mut sym_phase: Vec<f32> = Vec::with_capacity(n_symbols + 1);
    let mut acc = 0.0_f32;
    sym_phase.push(acc);
    for &d in dibits {
        let delta = match d & 0b11 {
            0b00 => PI / 4.0,
            0b01 => 3.0 * PI / 4.0,
            0b10 => -PI / 4.0,
            0b11 => -3.0 * PI / 4.0,
            _ => 0.0,
        };
        acc += delta;
        sym_phase.push(acc);
    }

    // Linear-interpolated complex baseband, one IQ sample per output tick.
    (0..total)
        .map(|i| {
            let frac = i as f32 / sps;
            let k = frac.floor() as usize;
            let mu = frac - k as f32;
            let p0 = sym_phase[k];
            let p1 = sym_phase[(k + 1).min(sym_phase.len() - 1)];
            let p = p0 + (p1 - p0) * mu;
            Complex32::new(p.cos(), p.sin())
        })
        .collect()
}

// ─── Stage emitters ─────────────────────────────────────────────────

/// /2 streaming decimator: 4096 samples in @ 62.5 kSPS → 2048 out @ 31.25 kSPS.
#[test]
fn emit_decimator_62k5_to_31k25() {
    let n = 4096;
    let input = frequency_sweep(n, 62_500.0);
    // Use the streaming decimator (the one the HDL must match).
    // Phase tracking is exercised by the existing
    // `streaming_decimator_preserves_phase_across_odd_chunks` test in
    // `lsm::filters::tests`; here we just run a single-chunk drive.
    let mut dec = StreamingDecimator2::new();
    let output = dec.process(&input);
    // Sanity check vs the batch helper.
    assert_eq!(output, decimate_by_2(&input));

    write_stage_iq(
        "decimator_62k5_to_31k25",
        "lsm::filters::StreamingDecimator2",
        62_500.0,
        31_250.0,
        &input,
        &output,
    );
}

/// 83-tap LPF: 2048 samples in/out at 31.25 kSPS, frequency-sweep input.
#[test]
fn emit_lpf_31250() {
    let n = 2048;
    let input = frequency_sweep(n, POST_DECIMATION_RATE_HZ);
    let mut fir = StreamingFir::new(&LPF_TAPS_31250);
    let output = fir.process(&input);
    // Sanity vs batch FIR (the streaming-vs-batch property is also
    // covered by an existing unit test in `lsm::filters::tests`).
    assert_eq!(output, apply_real_fir_complex(&LPF_TAPS_31250, &input));

    write_stage_iq(
        "lpf_31250",
        "lsm::filters::StreamingFir(LPF_TAPS_31250)",
        POST_DECIMATION_RATE_HZ,
        POST_DECIMATION_RATE_HZ,
        &input,
        &output,
    );
}

/// 105-tap RRC matched filter: same fixture shape as LPF but on the
/// RRC tap array.
#[test]
fn emit_rrc_31250() {
    let n = 2048;
    let input = frequency_sweep(n, POST_DECIMATION_RATE_HZ);
    let mut fir = StreamingFir::new(&RRC_TAPS_31250);
    let output = fir.process(&input);
    assert_eq!(output, apply_real_fir_complex(&RRC_TAPS_31250, &input));

    write_stage_iq(
        "rrc_31250",
        "lsm::filters::StreamingFir(RRC_TAPS_31250)",
        POST_DECIMATION_RATE_HZ,
        POST_DECIMATION_RATE_HZ,
        &input,
        &output,
    );
}

/// End-to-end: synthetic LSM baseband → LPF → RRC → demod loop.
///
/// Dumps the post-RRC IQ as input + the per-symbol soft/hard demod
/// output (soft phases, hard dibits, PLL trace, timing trace) so the
/// downstream HDL demod-loop test can verify slicer + PLL + Gardner
/// against a known reference sequence.
///
/// File extension over the generic IQ format:
///   "n_symbols", "soft_re", "soft_im", "soft_phase", "hard_dibit",
///   "pll", "sample_point", "truth_dibit"
#[test]
fn emit_demod_loop_synthetic() {
    // 256 dibits = enough to settle AGC + PLL + Gardner. Cycle through
    // all four constellation points so every quadrant is exercised.
    let dibits: Vec<u8> = (0..256).map(|i| (i % 4) as u8).collect();
    let synth = synth_lsm_iq(&dibits, POST_DECIMATION_RATE_HZ);

    // Run the synth IQ through the actual receive filters first so the
    // demod loop sees the same shape it would on-air.
    let lpf_out = apply_real_fir_complex(&LPF_TAPS_31250, &synth);
    let rrc_out = apply_real_fir_complex(&RRC_TAPS_31250, &lpf_out);

    let sps = POST_DECIMATION_RATE_HZ / P25_SYMBOL_RATE;
    let mut state = DemodState::new(sps);
    let demod = demod_lsm_with_state(&rrc_out, POST_DECIMATION_RATE_HZ, &mut state);

    // Hand-rolled JSON because this stage has more fields than the
    // generic IQ writer expects.
    let path = golden_path("demod_loop_synthetic");
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str("  \"name\": \"demod_loop_synthetic\",\n");
    s.push_str(
        "  \"stage\": \"lsm::demod::demod_lsm_with_state(synth_lsm + LPF + RRC)\",\n",
    );
    s.push_str(&format!(
        "  \"input_rate_hz\": {},\n",
        POST_DECIMATION_RATE_HZ
    ));
    s.push_str(&format!("  \"symbol_rate_hz\": {},\n", P25_SYMBOL_RATE));
    s.push_str(&format!("  \"samples_per_symbol\": {sps},\n"));
    s.push_str(&format!("  \"n_input\": {},\n", rrc_out.len()));
    s.push_str(&format!("  \"n_symbols\": {},\n", demod.n_symbols()));
    push_f32_array(&mut s, "input_re", rrc_out.iter().map(|c| c.re));
    s.push_str(",\n");
    push_f32_array(&mut s, "input_im", rrc_out.iter().map(|c| c.im));
    s.push_str(",\n");
    push_f32_array(&mut s, "soft_re", demod.soft_symbols.iter().map(|c| c.re));
    s.push_str(",\n");
    push_f32_array(&mut s, "soft_im", demod.soft_symbols.iter().map(|c| c.im));
    s.push_str(",\n");
    push_f32_array(&mut s, "soft_phase", demod.soft_phases.iter().copied());
    s.push_str(",\n");
    s.push_str("  \"hard_dibit\": [");
    let mut first = true;
    for &d in &demod.hard_dibits {
        if !first {
            s.push_str(", ");
        }
        first = false;
        s.push_str(&format!("{d}"));
    }
    s.push_str("],\n");
    push_f32_array(&mut s, "pll", demod.pll_trace.iter().copied());
    s.push_str(",\n");
    push_f32_array(&mut s, "sample_point", demod.timing_trace.iter().copied());
    s.push_str(",\n");
    // Echo the source dibit sequence so the HDL test can compute BER
    // against ground truth without re-deriving it.
    s.push_str("  \"truth_dibit\": [");
    let mut first = true;
    for &d in &dibits {
        if !first {
            s.push_str(", ");
        }
        first = false;
        s.push_str(&format!("{d}"));
    }
    s.push_str("]\n");
    s.push_str("}\n");
    fs::write(&path, s).expect("write golden file");
    eprintln!("[golden] wrote {}", path.display());
}
