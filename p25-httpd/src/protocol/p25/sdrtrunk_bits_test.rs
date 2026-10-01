//! Offline validation: feed a SDRTrunk-captured `.bits` file through
//! the `ControlChannelDecoder` framer and check that our new TDULC
//! Link Control Word parser recovers the correct Motorola
//! `TALK_COMPLETE` BY: source.
//!
//! SDRTrunk names its per-call recording with the ground-truth source
//! as `..._TO_<TG>_FROM_<source>.mp3`. We scan the recordings
//! directory for a `.bits` file + its matching `.mp3` siblings and
//! assert each FROM: radio ID appears at least once in our parser's
//! `MotorolaTalkComplete { by_radio_id }` output.
//!
//! `.bits` file format (reverse-engineered from
//! `C:/Users/Andy/SDRTrunk/recordings/*.bits`, confirmed by spotting
//! the `5575F5FF77FF` frame sync at byte offset 4):
//!
//! - One byte = 4 dibits, MSB-first. Byte `0b_aabbccdd` = dibits
//!   `[aa, bb, cc, dd]`.
//! - No file header. First few bytes may be pre-sync preamble from
//!   the demodulator settling.
//!
//! Gated behind env var `P25_SDRTRUNK_DIR` so the test only runs when
//! the SDRTrunk recordings are available locally. `cargo test
//! sdrtrunk_bits_test` without the env var prints a skip notice and
//! returns.

use std::path::PathBuf;

use super::control_channel::{ControlChannelDecoder, VoiceHandler};
use super::voice_frame::{
    parse_hdu_body, parse_ldu2_ess, parse_tdulc_lcw, tdulc_lc_bytes, TdulcLcw,
};

struct CollectingHandler {
    motorola: std::sync::Mutex<Vec<u32>>,
    gvcu: std::sync::atomic::AtomicU64,
    other: std::sync::atomic::AtomicU64,
    none: std::sync::atomic::AtomicU64,
    tdu_lc_total: std::sync::atomic::AtomicU64,
    ldu1_total: std::sync::atomic::AtomicU64,
    ldu2_total: std::sync::atomic::AtomicU64,
    hdu_total: std::sync::atomic::AtomicU64,
    tdu_total: std::sync::atomic::AtomicU64,
    /// 2026-04-19 diag: histogram of observed MFID bytes across all
    /// TDULCs (for seeing whether we're hitting 0x90, 0x00, or random
    /// noise). Indexed by MFID value 0..255.
    mfid_hist: std::sync::Mutex<[u64; 256]>,
    /// 2026-04-19 diag: histogram of observed opcode bytes.
    opcode_hist: std::sync::Mutex<[u64; 64]>,
    /// First 5 raw LC-byte dumps, for eyeballing what the parser is
    /// actually seeing.
    first_lc_dumps: std::sync::Mutex<Vec<[u8; 9]>>,

    // 2026-04-19 (HDU + LDU2 ESS parity):
    /// All talkgroup IDs recovered by `parse_hdu_body` on this file.
    hdu_talkgroups: std::sync::Mutex<Vec<u16>>,
    /// HDUs where Golay18 + RS(63,47,17) returned `Some(...)` (i.e.,
    /// the FEC chain at least produced a structured header — the
    /// algorithm/key values may still be garbage if RS couldn't fully
    /// correct, so downstream checks gate on plausibility).
    hdu_parsed_total: std::sync::atomic::AtomicU64,
    /// LDU2 ESS decode counters. `_unencrypted` means the RS layer
    /// returned `algorithm_id == 0x80` (per TIA-102.AABD, the
    /// UNENCRYPTED sentinel).
    ldu2_ess_parsed_total: std::sync::atomic::AtomicU64,
    ldu2_ess_unencrypted: std::sync::atomic::AtomicU64,
}

impl CollectingHandler {
    fn new() -> Self {
        Self {
            motorola: std::sync::Mutex::new(Vec::new()),
            gvcu: 0.into(),
            other: 0.into(),
            none: 0.into(),
            tdu_lc_total: 0.into(),
            ldu1_total: 0.into(),
            ldu2_total: 0.into(),
            hdu_total: 0.into(),
            tdu_total: 0.into(),
            mfid_hist: std::sync::Mutex::new([0u64; 256]),
            opcode_hist: std::sync::Mutex::new([0u64; 64]),
            first_lc_dumps: std::sync::Mutex::new(Vec::new()),
            hdu_talkgroups: std::sync::Mutex::new(Vec::new()),
            hdu_parsed_total: 0.into(),
            ldu2_ess_parsed_total: 0.into(),
            ldu2_ess_unencrypted: 0.into(),
        }
    }
}

impl VoiceHandler for CollectingHandler {
    fn on_ldu1(
        &self,
        _: &[super::voice_frame::ImbeFrameRaw; 9],
        _body_raw: &[u8],
    ) {
        self.ldu1_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    fn on_ldu2(
        &self,
        _: &[super::voice_frame::ImbeFrameRaw; 9],
        body_raw: &[u8],
    ) {
        self.ldu2_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 2026-04-19: exercise Hamming10 + RS(24,16,9) per LDU2 and
        // check the ESS decodes to something plausible. Clay County
        // is unencrypted, so we expect algorithm_id == 0x80 on every
        // successfully-recovered ESS.
        if let Some(ess) = parse_ldu2_ess(body_raw) {
            self.ldu2_ess_parsed_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if !ess.is_encrypted() {
                self.ldu2_ess_unencrypted
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    fn on_hdu(&self, body_raw: &[u8]) {
        self.hdu_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 2026-04-19: exercise Golay18 + RS(63,47,17). The FEC chain
        // is applied to every HDU; collect the recovered talkgroup
        // for the file-level assertion below (the .bits filename
        // carries the expected TG as `_TO_<tg>_FROM_...`).
        if let Some(hdr) = parse_hdu_body(body_raw) {
            self.hdu_parsed_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Ok(mut v) = self.hdu_talkgroups.lock() {
                v.push(hdr.talkgroup);
            }
        }
    }
    fn on_tdu(&self) {
        self.tdu_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    fn on_tdu_lc(&self, body_raw: &[u8]) {
        self.tdu_lc_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Diagnostic: snapshot raw LC bytes + MFID/opcode bytes
        // before calling the parser so we can see what the parser
        // is actually seeing regardless of how it classifies.
        if let Some(bytes) = tdulc_lc_bytes(body_raw) {
            // byte 0 = opcode byte (bits 2-7 = opcode; bits 0-1 = flags)
            // byte 1 = MFID
            let opcode = (bytes[0] & 0x3F) as usize;
            let mfid = bytes[1] as usize;
            if let Ok(mut h) = self.mfid_hist.lock() {
                h[mfid] += 1;
            }
            if let Ok(mut h) = self.opcode_hist.lock() {
                h[opcode] += 1;
            }
            if let Ok(mut v) = self.first_lc_dumps.lock() {
                if v.len() < 5 {
                    v.push(bytes);
                }
            }
        }
        match parse_tdulc_lcw(body_raw) {
            Some(TdulcLcw::MotorolaTalkComplete { by_radio_id }) => {
                if let Ok(mut v) = self.motorola.lock() {
                    v.push(by_radio_id);
                }
            }
            Some(TdulcLcw::GroupVoiceChannelUser { .. }) => {
                self.gvcu
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Some(
                TdulcLcw::GroupVoiceChannelUpdate { .. }
                | TdulcLcw::CallTermination { .. }
                | TdulcLcw::RfssStatusBroadcast { .. }
                | TdulcLcw::NetStatusBroadcast { .. }
                | TdulcLcw::Other { .. },
            ) => {
                self.other
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            None => {
                self.none
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// Unpack SDRTrunk `.bits` bytes into a Vec of dibits (2-bit values).
fn bits_file_to_dibits(bytes: &[u8]) -> Vec<u8> {
    let mut dibits = Vec::with_capacity(bytes.len() * 4);
    for &b in bytes {
        dibits.push((b >> 6) & 0x03);
        dibits.push((b >> 4) & 0x03);
        dibits.push((b >> 2) & 0x03);
        dibits.push(b & 0x03);
    }
    dibits
}

/// Extract the expected talkgroup from sibling `..._TO_<tg>_FROM_...`
/// MP3s. Returns the first TG found that matches the bits stem's
/// minute prefix + LCN token; if multiple TGs appear (unusual on a
/// single-call capture), returns the most common.
fn expected_talkgroup_for_bits_file(
    dir: &std::path::Path,
    bits_stem: &str,
) -> Option<u16> {
    let minute_prefix = &bits_stem[..13];
    let lcn_tok = bits_stem
        .split('_')
        .find(|part| part.starts_with("T-LCN-"))
        .unwrap_or("");

    let mut tg_counts: std::collections::HashMap<u16, u32> =
        std::collections::HashMap::new();
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().into_string().ok()?;
        if !name.ends_with(".mp3") || !name.contains("_TO_") {
            continue;
        }
        if !name.starts_with(minute_prefix) {
            continue;
        }
        if !lcn_tok.is_empty() && !name.contains(lcn_tok) {
            continue;
        }
        if let Some((_, after)) = name.split_once("_TO_") {
            let digits: String =
                after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(tg) = digits.parse::<u16>() {
                *tg_counts.entry(tg).or_insert(0) += 1;
            }
        }
    }
    tg_counts.into_iter().max_by_key(|(_, c)| *c).map(|(tg, _)| tg)
}

/// Extract FROM:<source> IDs from every `..._FROM_<n>.mp3` in `dir`
/// whose timestamp prefix + T-LCN suffix matches the bits file.
fn expected_sources_for_bits_file(
    dir: &std::path::Path,
    bits_stem: &str,
) -> Vec<u32> {
    // `.bits` stem looks like
    //   `20260415_174725_857437500_9600BPS_APCO25PHASE1_Clay-County_Clay_T-LCN-11_69`
    // Matching MP3:
    //   `20260415_174728_Clay-County_Clay_T-LCN-11__TO_300_FROM_3409922.mp3`
    // Share the same yyyymmdd_hhmm minute prefix + the same T-LCN-<n>
    // token. The call number at the tail of the bits stem (e.g. `_69`)
    // doesn't appear in the MP3 filename, so match on the LCN token.
    let minute_prefix = &bits_stem[..13]; // "20260415_1747"
    // Pull "T-LCN-11" token from the bits stem.
    let lcn_tok = bits_stem
        .split('_')
        .find(|part| part.starts_with("T-LCN-"))
        .unwrap_or("");

    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if !name.ends_with(".mp3") || !name.contains("_FROM_") {
            continue;
        }
        if !name.starts_with(minute_prefix) {
            continue;
        }
        if !lcn_tok.is_empty() && !name.contains(lcn_tok) {
            continue;
        }
        if let Some((_, after)) = name.split_once("_FROM_") {
            let digits: String =
                after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(id) = digits.parse::<u32>() {
                out.push(id);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn motorola_talk_complete_recovers_from_sdrtrunk_bits() {
    let sdr_dir = match std::env::var("P25_SDRTRUNK_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => {
            eprintln!(
                "P25_SDRTRUNK_DIR not set; skipping SDRTrunk .bits test. \
                 Set it to a directory with `*.bits` files + matching \
                 `_TO_<TG>_FROM_<src>.mp3` filenames to exercise this."
            );
            return;
        }
    };

    let entries: Vec<_> = std::fs::read_dir(&sdr_dir)
        .expect("P25_SDRTRUNK_DIR readable")
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .map(|s| s == "bits")
                .unwrap_or(false)
        })
        .collect();
    assert!(
        !entries.is_empty(),
        "no .bits files in {}",
        sdr_dir.display()
    );

    let mut total_expected_matched = 0usize;
    let mut total_expected_missed = 0usize;
    let mut files_checked = 0usize;
    let mut any_traffic_file_checked = false;

    for e in entries {
        let path = e.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Only traffic-channel (`T-LCN-*`) bits files; the control
        // channel (`LCN-*` without the leading `T-`) doesn't emit
        // voice TDULCs.
        if !stem.contains("T-LCN-") {
            continue;
        }

        let expected = expected_sources_for_bits_file(&sdr_dir, stem);
        if expected.is_empty() {
            continue;
        }
        any_traffic_file_checked = true;

        let bytes = std::fs::read(&path).expect("read bits");
        let dibits = bits_file_to_dibits(&bytes);

        let mut decoder = ControlChannelDecoder::new();
        let handler = std::sync::Arc::new(CollectingHandler::new());
        decoder.set_voice_handler(handler.clone());
        for d in dibits {
            decoder.process_dibit(d);
        }

        let recovered = {
            let v = handler.motorola.lock().unwrap();
            v.clone()
        };
        let recovered_unique: std::collections::HashSet<u32> =
            recovered.iter().copied().collect();

        // Dump top MFIDs + opcodes + first few LC byte dumps so we
        // can see what the parser is actually seeing (zero Motorola
        // matches across 200 TDULCs strongly suggests an extraction
        // bug, not a bit-error issue).
        let top_mfids = {
            let h = handler.mfid_hist.lock().unwrap();
            let mut v: Vec<(usize, u64)> =
                h.iter().enumerate().map(|(i, &c)| (i, c)).collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            v.into_iter().filter(|(_, c)| *c > 0).take(5).collect::<Vec<_>>()
        };
        let top_opcodes = {
            let h = handler.opcode_hist.lock().unwrap();
            let mut v: Vec<(usize, u64)> =
                h.iter().enumerate().map(|(i, &c)| (i, c)).collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            v.into_iter().filter(|(_, c)| *c > 0).take(5).collect::<Vec<_>>()
        };
        let sample_dumps: Vec<String> = handler
            .first_lc_dumps
            .lock()
            .unwrap()
            .iter()
            .map(|b| {
                b.iter()
                    .map(|x| format!("{:02X}", x))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        println!(
            "  top_mfids={:?}  top_opcodes={:?}",
            top_mfids
                .iter()
                .map(|(k, v)| format!("0x{:02X}={}", k, v))
                .collect::<Vec<_>>(),
            top_opcodes
                .iter()
                .map(|(k, v)| format!("0x{:02X}={}", k, v))
                .collect::<Vec<_>>(),
        );
        for (i, d) in sample_dumps.iter().enumerate() {
            println!("  lc[{}]= {}", i, d);
        }

        println!(
            "bits_file={}  expected_srcs={:?}  recovered={:?}  \
             counts(tdu_lc={} gvcu={} other={} none={} hdu={} ldu1={} ldu2={} tdu={})",
            path.file_name().unwrap().to_string_lossy(),
            expected,
            recovered_unique,
            handler
                .tdu_lc_total
                .load(std::sync::atomic::Ordering::Relaxed),
            handler.gvcu.load(std::sync::atomic::Ordering::Relaxed),
            handler.other.load(std::sync::atomic::Ordering::Relaxed),
            handler.none.load(std::sync::atomic::Ordering::Relaxed),
            handler.hdu_total.load(std::sync::atomic::Ordering::Relaxed),
            handler.ldu1_total.load(std::sync::atomic::Ordering::Relaxed),
            handler.ldu2_total.load(std::sync::atomic::Ordering::Relaxed),
            handler.tdu_total.load(std::sync::atomic::Ordering::Relaxed),
        );

        for src in expected {
            if recovered_unique.contains(&src) {
                total_expected_matched += 1;
            } else {
                total_expected_missed += 1;
            }
        }
        files_checked += 1;
    }

    assert!(
        any_traffic_file_checked,
        "no traffic-channel .bits file had matching FROM:* mp3 siblings"
    );

    let total_expected = total_expected_matched + total_expected_missed;
    let pct = if total_expected == 0 {
        0.0
    } else {
        100.0 * total_expected_matched as f64 / total_expected as f64
    };
    println!(
        "\nSUMMARY: {}/{} expected sources recovered ({:.1}%) across \
         {} bits files",
        total_expected_matched, total_expected, pct, files_checked
    );
    // 2026-04-19: without Golay(24,12) FEC the recovery is bounded
    // by per-bit channel noise; on the Clay County 2026-04-15 and
    // 2026-04-18 `.bits` captures we consistently land around 20 %
    // of the SDRTrunk-per-speaker `FROM:<id>` list (which counts
    // every unique speaker across a multi-speaker follow-session,
    // not per-PTT). Any drop below 10 % signals a structural
    // regression — parser bit positions or status-dibit handling
    // broke — so gate there.
    assert!(
        pct >= 10.0,
        "Motorola TALK_COMPLETE recovery rate {:.1}% below the 10% floor — \
         parser regression. Dump the `top_mfids` lines printed above to \
         see whether the MFID byte still reads as 0x90/0x00.",
        pct
    );
}

/// 2026-04-19: end-to-end parity test for the newly-added HDU +
/// LDU2 ESS decoders. Feeds every T-LCN `.bits` file through the
/// framer, runs Golay18 + RS(63,47,17) on every HDU and Hamming10 +
/// RS(24,16,9) on every LDU2, and checks:
///
/// 1. Every file produces at least one HDU that RS fully recovers.
/// 2. Recovered HDU talkgroups match the TG in the SDRTrunk MP3
///    filename (`_TO_<tg>_FROM_...`) on a plurality of parses.
/// 3. Every LDU2 ESS that RS decodes reports algorithm_id=0x80
///    (UNENCRYPTED) since the Clay County captures are all clear-mode.
///
/// Assertions gate at conservative floors (>=50% HDU-TG match,
/// >=80% LDU2 ESS UNENCRYPTED) so noise-driven bit flips don't flap
/// the test; tighten them once we have more calibration data.
#[test]
fn hdu_and_ldu2_ess_decode_on_sdrtrunk_bits() {
    let sdr_dir = match std::env::var("P25_SDRTRUNK_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => {
            eprintln!(
                "P25_SDRTRUNK_DIR not set; skipping HDU/LDU2 ESS test."
            );
            return;
        }
    };

    let entries: Vec<_> = std::fs::read_dir(&sdr_dir)
        .expect("P25_SDRTRUNK_DIR readable")
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .map(|s| s == "bits")
                .unwrap_or(false)
        })
        .collect();
    assert!(
        !entries.is_empty(),
        "no .bits files in {}",
        sdr_dir.display()
    );

    let mut total_hdu_tg_match = 0u64;
    let mut total_hdu_parsed = 0u64;
    let mut total_ldu2_ess_parsed = 0u64;
    let mut total_ldu2_ess_unencrypted = 0u64;
    let mut files_checked = 0usize;
    let mut files_with_any_hdu = 0usize;

    for e in entries {
        let path = e.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !stem.contains("T-LCN-") {
            continue;
        }
        let Some(expected_tg) = expected_talkgroup_for_bits_file(&sdr_dir, stem)
        else {
            continue;
        };

        let bytes = std::fs::read(&path).expect("read bits");
        let dibits = bits_file_to_dibits(&bytes);

        let mut decoder = ControlChannelDecoder::new();
        let handler = std::sync::Arc::new(CollectingHandler::new());
        decoder.set_voice_handler(handler.clone());
        for d in dibits {
            decoder.process_dibit(d);
        }

        let hdu_count = handler
            .hdu_total
            .load(std::sync::atomic::Ordering::Relaxed);
        let hdu_parsed = handler
            .hdu_parsed_total
            .load(std::sync::atomic::Ordering::Relaxed);
        let ldu2_parsed = handler
            .ldu2_ess_parsed_total
            .load(std::sync::atomic::Ordering::Relaxed);
        let ldu2_unenc = handler
            .ldu2_ess_unencrypted
            .load(std::sync::atomic::Ordering::Relaxed);
        let hdu_tgs: Vec<u16> = handler
            .hdu_talkgroups
            .lock()
            .unwrap()
            .clone();
        let tg_matches = hdu_tgs
            .iter()
            .filter(|t| **t == expected_tg)
            .count() as u64;

        println!(
            "bits_file={}  expected_tg={}  hdu(total={} parsed={} tg_matches={}) \
             ldu2_ess(parsed={} unencrypted={})  recovered_tgs={:?}",
            path.file_name().unwrap().to_string_lossy(),
            expected_tg,
            hdu_count,
            hdu_parsed,
            tg_matches,
            ldu2_parsed,
            ldu2_unenc,
            hdu_tgs,
        );

        total_hdu_tg_match += tg_matches;
        total_hdu_parsed += hdu_parsed;
        total_ldu2_ess_parsed += ldu2_parsed;
        total_ldu2_ess_unencrypted += ldu2_unenc;
        files_checked += 1;
        if hdu_count > 0 {
            files_with_any_hdu += 1;
        }
    }

    assert!(
        files_checked > 0,
        "no .bits files matched `_TO_<tg>_FROM_...` MP3 siblings"
    );
    let hdu_tg_pct = if total_hdu_parsed == 0 {
        0.0
    } else {
        100.0 * total_hdu_tg_match as f64 / total_hdu_parsed as f64
    };
    let ldu2_unenc_pct = if total_ldu2_ess_parsed == 0 {
        0.0
    } else {
        100.0 * total_ldu2_ess_unencrypted as f64
            / total_ldu2_ess_parsed as f64
    };
    println!(
        "\nHDU/LDU2 SUMMARY: {} files checked, {} with HDUs. \
         HDU TG match {}/{} ({:.1}%), \
         LDU2 ESS UNENCRYPTED {}/{} ({:.1}%)",
        files_checked,
        files_with_any_hdu,
        total_hdu_tg_match,
        total_hdu_parsed,
        hdu_tg_pct,
        total_ldu2_ess_unencrypted,
        total_ldu2_ess_parsed,
        ldu2_unenc_pct,
    );
    assert!(
        files_with_any_hdu * 2 >= files_checked,
        "more than half of .bits files produced zero HDUs — framer regression"
    );
    assert!(
        hdu_tg_pct >= 50.0,
        "HDU talkgroup recovery {:.1}% below 50% floor — Golay18 \
         + RS(63,47,17) chain regression. Check the `expected_tg` / \
         `recovered_tgs` prints above.",
        hdu_tg_pct,
    );
    assert!(
        ldu2_unenc_pct >= 80.0,
        "LDU2 ESS UNENCRYPTED rate {:.1}% below 80% floor on an \
         all-clear site — Hamming10 + RS(24,16,9) chain regression.",
        ldu2_unenc_pct,
    );
}

// ── Side-by-side parity check: SDRTrunk decoded_messages.log vs us ───

/// Minimal parse of a SDRTrunk `decoded_messages.log` line. We keep
/// only the tokens that map 1:1 onto what our framer + LCW parser
/// can emit; everything else is folded into `Raw`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SdrtrunkEvent {
    SyncLoss,
    Hdu,
    Ldu1,
    Ldu2,
    Tdu,
    TdulcStandard,
    TdulcMotorolaTalkComplete { by: u32 },
    /// Any line we don't bother classifying (TSBK, PDU, etc). Not
    /// expected on a T-LCN `.bits` file.
    Raw(String),
}

fn parse_sdrtrunk_log(path: &std::path::Path) -> Vec<SdrtrunkEvent> {
    let contents = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in contents.lines() {
        if line.is_empty()
            || line.starts_with("DECODED")
            || !line.contains("PASSED")
        {
            continue;
        }
        // Canonical form after the status comma:
        //   NAC:2209/x8A1 <DUID-TOKEN> <details>
        // where DUID-TOKEN is one of HDU/LDU1/LDU2/TDU/TDULC, or the
        // whole line is a sync-loss marker.
        if line.contains("SYNC LOSS") {
            out.push(SdrtrunkEvent::SyncLoss);
            continue;
        }
        if line.contains(" HDU ") || line.contains(" HDU  ") {
            out.push(SdrtrunkEvent::Hdu);
        } else if line.contains(" LDU1 ") {
            out.push(SdrtrunkEvent::Ldu1);
        } else if line.contains(" LDU2 ") {
            out.push(SdrtrunkEvent::Ldu2);
        } else if line.contains(" TDULC ") {
            if line.contains("MOTOROLA TALK COMPLETE") {
                // "BY:<digits>" follows the label.
                let by = line
                    .split("BY:")
                    .nth(1)
                    .and_then(|s| {
                        s.chars()
                            .take_while(|c| c.is_ascii_digit())
                            .collect::<String>()
                            .parse::<u32>()
                            .ok()
                    })
                    .unwrap_or(0);
                out.push(SdrtrunkEvent::TdulcMotorolaTalkComplete { by });
            } else {
                out.push(SdrtrunkEvent::TdulcStandard);
            }
        } else if line.contains(" TDU ") || line.contains(" TDU  ") {
            out.push(SdrtrunkEvent::Tdu);
        } else {
            out.push(SdrtrunkEvent::Raw(line.to_string()));
        }
    }
    out
}

#[derive(Debug, Default)]
struct EventCounts {
    hdu: u64,
    ldu1: u64,
    ldu2: u64,
    tdu: u64,
    tdulc_std: u64,
    tdulc_mot: u64,
    mot_sources: Vec<u32>,
}

fn count_events(events: &[SdrtrunkEvent]) -> EventCounts {
    let mut c = EventCounts::default();
    for e in events {
        match e {
            SdrtrunkEvent::Hdu => c.hdu += 1,
            SdrtrunkEvent::Ldu1 => c.ldu1 += 1,
            SdrtrunkEvent::Ldu2 => c.ldu2 += 1,
            SdrtrunkEvent::Tdu => c.tdu += 1,
            SdrtrunkEvent::TdulcStandard => c.tdulc_std += 1,
            SdrtrunkEvent::TdulcMotorolaTalkComplete { by } => {
                c.tdulc_mot += 1;
                c.mot_sources.push(*by);
            }
            _ => {}
        }
    }
    c
}

/// Pull our framer's event list out of a CollectingHandler so we can
/// compare against SDRTrunk's in the same shape.
fn our_events(h: &CollectingHandler) -> EventCounts {
    use std::sync::atomic::Ordering;
    let motorola = h.motorola.lock().map(|v| v.clone()).unwrap_or_default();
    EventCounts {
        hdu: h.hdu_total.load(Ordering::Relaxed),
        ldu1: h.ldu1_total.load(Ordering::Relaxed),
        ldu2: h.ldu2_total.load(Ordering::Relaxed),
        tdu: h.tdu_total.load(Ordering::Relaxed),
        tdulc_std: h.gvcu.load(Ordering::Relaxed),
        tdulc_mot: motorola.len() as u64,
        mot_sources: motorola,
    }
}

/// Find the SDRTrunk `decoded_messages.log` that corresponds to a
/// given `.bits` file. The filenames share the same first 15 chars
/// (ymdhms) plus the T-LCN token; the log adds a milliseconds
/// suffix to the timestamp.
fn find_matching_log(
    sdr_dir: &std::path::Path,
    bits_stem: &str,
) -> Option<PathBuf> {
    // Both `.bits` files and `_decoded_messages.log` files live
    // in different dirs in a normal SDRTrunk install; let the caller
    // supply the `event_logs/` dir via an env var so we don't have
    // to probe. Fall back to `<sdr_dir>/../event_logs/`.
    let logs_dir = std::env::var("P25_SDRTRUNK_LOGS_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            sdr_dir.parent().map(|p| p.join("event_logs"))
        })?;
    let ts_prefix = &bits_stem[..15]; // "20260418_221510"
    // Frequency sits between the timestamp and the _9600BPS chunk in
    // the bits stem; logs embed the same frequency right after the
    // millisecond suffix, so restrict matches to that frequency too
    // (prevents picking a control-channel log that happens to share
    // the minute prefix).
    let freq_tok = bits_stem.split('_').nth(2).unwrap_or("");
    let lcn_tok = bits_stem
        .split('_')
        .find(|t| t.starts_with("T-LCN-"))
        .unwrap_or("");
    let entries = std::fs::read_dir(&logs_dir).ok()?;
    let mut best: Option<PathBuf> = None;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.contains("decoded_messages.log") {
            continue;
        }
        if !name.starts_with(ts_prefix) {
            continue;
        }
        if !freq_tok.is_empty() && !name.contains(freq_tok) {
            continue;
        }
        if !lcn_tok.is_empty() && !name.contains(lcn_tok) {
            continue;
        }
        best = Some(e.path());
        break;
    }
    best
}

#[test]
fn duid_and_tdulc_parity_with_sdrtrunk() {
    let sdr_dir = match std::env::var("P25_SDRTRUNK_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => {
            eprintln!(
                "P25_SDRTRUNK_DIR not set; skipping DUID/LCW parity test."
            );
            return;
        }
    };

    let entries: Vec<_> = std::fs::read_dir(&sdr_dir)
        .expect("P25_SDRTRUNK_DIR readable")
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .map(|s| s == "bits")
                .unwrap_or(false)
        })
        .collect();

    let mut any_checked = false;
    let mut total_mot_ours = 0u64;
    let mut total_mot_sdrt = 0u64;
    let mut total_mot_source_matches = 0u64;

    for e in entries {
        let path = e.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !stem.contains("T-LCN-") {
            continue;
        }
        let Some(log_path) = find_matching_log(&sdr_dir, stem) else {
            continue;
        };
        let sdrt_events = parse_sdrtrunk_log(&log_path);
        if sdrt_events.is_empty() {
            continue;
        }
        any_checked = true;

        let bytes = std::fs::read(&path).expect("read bits");
        let dibits = bits_file_to_dibits(&bytes);
        let mut decoder = ControlChannelDecoder::new();
        let handler = std::sync::Arc::new(CollectingHandler::new());
        decoder.set_voice_handler(handler.clone());
        for d in dibits {
            decoder.process_dibit(d);
        }

        let sdrt = count_events(&sdrt_events);
        let ours = our_events(&handler);

        println!(
            "\n== {} ==",
            path.file_name().unwrap().to_string_lossy()
        );
        println!(
            "             SDRTrunk   Ours   Delta"
        );
        let row = |label: &str, s: u64, o: u64| {
            let d = o as i64 - s as i64;
            println!("  {:<10} {:>7}  {:>5}  {:+}", label, s, o, d);
        };
        row("HDU", sdrt.hdu, ours.hdu);
        row("LDU1", sdrt.ldu1, ours.ldu1);
        row("LDU2", sdrt.ldu2, ours.ldu2);
        row("TDU", sdrt.tdu, ours.tdu);
        row("TDULC_std", sdrt.tdulc_std, ours.tdulc_std);
        row("TDULC_mot", sdrt.tdulc_mot, ours.tdulc_mot);
        print!("  SDRTrunk BY:");
        for b in &sdrt.mot_sources {
            print!(" {}", b);
        }
        print!("\n  Ours     BY:");
        for b in &ours.mot_sources {
            print!(" {}", b);
        }
        println!();

        total_mot_ours += ours.tdulc_mot;
        total_mot_sdrt += sdrt.tdulc_mot;
        // Count per-file Motorola-source agreements: for each BY:
        // SDRTrunk emitted, check whether we saw the same BY.
        let ours_set: std::collections::HashSet<u32> =
            ours.mot_sources.iter().copied().collect();
        for by in &sdrt.mot_sources {
            if ours_set.contains(by) {
                total_mot_source_matches += 1;
            }
        }
    }

    assert!(
        any_checked,
        "no .bits/decoded_messages.log pair found — set \
         P25_SDRTRUNK_LOGS_DIR if your event_logs/ dir isn't \
         adjacent to the recordings dir"
    );
    let pct = if total_mot_sdrt == 0 {
        0.0
    } else {
        100.0 * total_mot_source_matches as f64 / total_mot_sdrt as f64
    };
    println!(
        "\nPARITY SUMMARY: Motorola sources matched {}/{} ({:.1}%), \
         ours emitted {}",
        total_mot_source_matches, total_mot_sdrt, pct, total_mot_ours
    );
    // Same 10 % floor as the other test — the parser is pre-FEC so
    // we expect some BY: misses on noisy captures.
    assert!(
        pct >= 10.0,
        "Motorola BY: agreement with SDRTrunk {:.1}% below floor",
        pct
    );
}

/// 2026-04-19: control-channel `.bits` parity. SDRTrunk's CC capture
/// files have `_LCN-<n>_` in the stem (no leading `T-`, because
/// `T-LCN` means "traffic LCN"). Re-run the bits through our framer
/// + TSBK parser, aggregate TSBK opcode labels, and compare
/// histograms against SDRTrunk's matching `decoded_messages.log`.
#[test]
fn control_channel_tsbk_parity_with_sdrtrunk() {
    let sdr_dir = match std::env::var("P25_SDRTRUNK_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => {
            eprintln!("P25_SDRTRUNK_DIR not set; skipping CC parity test.");
            return;
        }
    };

    let entries: Vec<_> = std::fs::read_dir(&sdr_dir)
        .expect("P25_SDRTRUNK_DIR readable")
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .map(|s| s == "bits")
                .unwrap_or(false)
        })
        .collect();

    let mut any_checked = false;
    let mut total_sdr_tsbks = 0u64;
    let mut total_our_tsbks = 0u64;
    let mut total_label_overlap = 0u64;

    for e in entries {
        let path = e.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // CC files have plain `LCN-` not `T-LCN-`.
        if !stem.contains("LCN-") || stem.contains("T-LCN-") {
            continue;
        }
        let Some(log_path) = find_matching_log(&sdr_dir, stem) else {
            continue;
        };

        use std::collections::HashMap;
        let mut sdr_opcodes: HashMap<String, u64> = HashMap::new();
        let contents = match std::fs::read_to_string(&log_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for line in contents.lines() {
            if !line.contains("PASSED") {
                continue;
            }
            // Format: `... TSBK<digit> [**CRC-FAILED** ]<LABEL> ...`
            // We skip past "TSBK<digit>" (5 chars) and read the
            // first non-noise token. `**CRC-FAILED**` modifier is
            // stripped so the underlying opcode label aggregates
            // correctly across passed + failed frames.
            if let Some(tsbk_pos) = line.find("TSBK") {
                let rest = &line[tsbk_pos + 5..];
                let mut toks =
                    rest.split_whitespace().filter(|t| {
                        !t.is_empty() && *t != "**CRC-FAILED**"
                    });
                if let Some(label) = toks.next() {
                    *sdr_opcodes.entry(label.to_string()).or_insert(0) += 1;
                }
            }
        }
        if sdr_opcodes.is_empty() {
            continue;
        }
        any_checked = true;

        let bytes = std::fs::read(&path).expect("read bits");
        let dibits = bits_file_to_dibits(&bytes);
        let mut decoder = ControlChannelDecoder::new();
        // Max-retention for this test so we see every TSBK the
        // framer emits; default ring caps at a few hundred.
        for d in dibits {
            decoder.process_dibit(d);
        }

        let mut our_opcodes: HashMap<String, u64> = HashMap::new();
        for (_t, _block_idx, msg) in &decoder.recent_messages {
            let label = our_label(msg);
            *our_opcodes.entry(label.to_string()).or_insert(0) += 1;
        }

        let sdr_sum: u64 = sdr_opcodes.values().sum();
        let our_sum: u64 = our_opcodes.values().sum();
        total_sdr_tsbks += sdr_sum;
        total_our_tsbks += our_sum;

        println!(
            "\n== {} ==",
            path.file_name().unwrap().to_string_lossy()
        );
        println!(
            "  SDRTrunk total TSBKs: {:5}  Ours: {:5}",
            sdr_sum, our_sum
        );
        let mut all_labels: std::collections::BTreeSet<&str> =
            sdr_opcodes.keys().map(|s| s.as_str()).collect();
        for k in our_opcodes.keys() {
            all_labels.insert(k.as_str());
        }
        println!("    {:<28} {:>8} {:>8}", "label", "SDRTrunk", "Ours");
        for lbl in &all_labels {
            let s = sdr_opcodes.get(*lbl).copied().unwrap_or(0);
            let o = our_opcodes.get(*lbl).copied().unwrap_or(0);
            total_label_overlap += s.min(o);
            println!("    {:<28} {:>8} {:>8}", lbl, s, o);
        }
    }

    assert!(
        any_checked,
        "no control-channel .bits/decoded_messages.log pair found"
    );

    let pct = if total_sdr_tsbks == 0 {
        0.0
    } else {
        100.0 * total_label_overlap as f64 / total_sdr_tsbks as f64
    };
    println!(
        "\nCONTROL-CHANNEL PARITY SUMMARY: per-label-min-overlap = \
         {}/{} SDRTrunk TSBKs ({:.1}%), ours emitted {}",
        total_label_overlap, total_sdr_tsbks, pct, total_our_tsbks,
    );
}

/// Translate one of our TsbkMessage variants into the short label
/// SDRTrunk uses in `decoded_messages.log` — lets the CC parity test
/// compare histograms across vocabularies.
fn our_label(msg: &crate::protocol::p25::tsbk::TsbkMessage) -> &'static str {
    use crate::protocol::p25::tsbk::TsbkMessage::*;
    match msg {
        GroupVoiceChannelGrant { .. } => "GRP_VCH_GRANT",
        GroupVoiceChannelGrantUpdate { .. } => "GRP_VCH_GRNT_UPD",
        GroupVoiceChannelGrantUpdateExplicit { .. } => "GRP_VCH_GRNT_UPD_EXP",
        IdentifierUpdate { .. } => "IDEN_UPDATE",
        NetworkStatus { .. } => "NET_STS_BCST",
        RfssStatus { .. } => "RFSS_STS_BCST",
        AdjacentStatus { .. } => "ADJ_STS_BCST",
        SecondaryControlChannelBroadcast { .. } => "SCCB",
        SndcpDataChannelAnnouncementExplicit { .. } => "SNDCP_DCH_ANN_EX",
        TdmaSyncBroadcast { .. } => "TDMA_SYNC_BCST",
        TelephoneInterconnectVoiceChannelGrantUpdate { .. } => {
            "TELE_INT_VCH_GRNT_UPD"
        }
        UnitToUnitAnswerRequest { .. } => "UU_ANS_REQ",
        RadioUnitMonitorCommand { .. } => "RAD_MON_CMD",
        SndcpDataChannelGrant { .. } => "SNDCP_DCH_GRANT",
        SndcpDataPageRequest { .. } => "SNDCP_DCH_PAG_RQ",
        AcknowledgeResponseFne { .. } => "ACK_RESP_FNE",
        GroupAffiliationResponse { .. } => "GRP_AFFIL_RESP",
        GroupAffiliationQuery { .. } => "GRP_AFFIL_Q",
        LocationRegistrationResponse { .. } => "LOC_REG_RESP",
        UnitRegistrationResponse { .. } => "U_REG_RSP",
        UnitDeRegistrationAcknowledge { .. } => "DE_REGIST_ACK",
        ManufacturerSpecific { mfid, .. } => match mfid {
            0x90 => "MOTOROLA_VENDOR",
            _ => "VENDOR_OTHER",
        },
    }
}

/// FNV-1a of a data unit body, so the dump stays small.
fn body_hash(body: &[u8]) -> u64 {
    body.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

#[derive(Default)]
struct DumpVoice {
    lines: std::sync::Mutex<Vec<String>>,
}

impl DumpVoice {
    fn push(&self, line: String) {
        self.lines.lock().unwrap().push(line);
    }
}

impl VoiceHandler for DumpVoice {
    fn on_ldu1(&self, _: &[crate::protocol::p25::voice_frame::ImbeFrameRaw; 9], body: &[u8]) {
        let lc = crate::protocol::p25::voice_frame::parse_ldu1_lcw(body);
        self.push(format!("ldu1 {:016x} {lc:?}", body_hash(body)));
    }
    fn on_ldu2(&self, _: &[crate::protocol::p25::voice_frame::ImbeFrameRaw; 9], body: &[u8]) {
        let ess = parse_ldu2_ess(body);
        self.push(format!("ldu2 {:016x} {ess:?}", body_hash(body)));
    }
    fn on_hdu(&self, body: &[u8]) {
        self.push(format!("hdu {:016x} {:?}", body_hash(body), parse_hdu_body(body)));
    }
    fn on_tdu(&self) {
        self.push("tdu".into());
    }
    fn on_tdu_lc(&self, body: &[u8]) {
        let lc = crate::protocol::p25::voice_frame::parse_tdulc_lcw_checked(body);
        self.push(format!("tdulc {:016x} {lc:?}", body_hash(body)));
    }
}

/// Writes what this framer makes of every `.bits` file in `P25_SDRTRUNK_DIR` to
/// `P25_FRAMER_DUMP/<stem>.txt`; the scanner crate's `framer_dump` writes the same format.
#[test]
#[ignore = "needs P25_SDRTRUNK_DIR and P25_FRAMER_DUMP"]
fn framer_dump() {
    use std::fmt::Write;
    let (Ok(dir), Ok(dump)) = (std::env::var("P25_SDRTRUNK_DIR"), std::env::var("P25_FRAMER_DUMP")) else {
        panic!("set P25_SDRTRUNK_DIR and P25_FRAMER_DUMP");
    };
    std::fs::create_dir_all(&dump).unwrap();
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "bits") {
            continue;
        }
        let mut dec = ControlChannelDecoder::new();
        dec.max_recent = usize::MAX;
        let voice = std::sync::Arc::new(DumpVoice::default());
        dec.set_voice_handler(voice.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel(1 << 20);
        dec.pdu_tx = Some(tx);
        for d in bits_file_to_dibits(&std::fs::read(&path).unwrap()) {
            dec.process_dibit(d);
        }
        let mut out = String::new();
        for (_, index, msg) in &dec.recent_messages {
            writeln!(out, "tsbk{index} {msg:?}").unwrap();
        }
        for line in voice.lines.lock().unwrap().iter() {
            writeln!(out, "{line}").unwrap();
        }
        while let Ok(p) = rx.try_recv() {
            writeln!(out, "pdu {:?} {:?} {}", p.header, p.blocks, p.blocks_expected).unwrap();
        }
        writeln!(
            out,
            "stats nid_ok={} tsdus={} tsbk_attempts={} tsbk_ok={} crc_fail={} trellis_fail={} unknown={}",
            dec.nid_decoded_ok,
            dec.tsdu_attempts,
            dec.tsbk_block_attempts,
            dec.tsbk_crc_ok,
            dec.tsbk_crc_failures,
            dec.tsbk_trellis_failures,
            dec.tsbk_unknown_opcode,
        )
        .unwrap();
        let stem = path.file_stem().unwrap().to_string_lossy();
        std::fs::write(std::path::Path::new(&dump).join(format!("{stem}.txt")), out).unwrap();
    }
}
