//! PN9 / PN11 sequences of the ADI `axi_ad9361` DAC PN generator and the
//! `pn1fn` path of the RX PN monitor (axi_ad9361_{tx_channel,rx_pnmon}.v).
//! Channel 0 (I) uses PRBS_SEL = CHANNEL_ID = 0 -> P09, channel 1 (Q) -> P11.
//!
//! Used by `txlink --mode fpga-loopback`: the ADI monitor taps the raw ADC
//! data *before* the DATA_SEL loopback mux, so looped-back DAC PN has to be
//! checked in software on captured samples.

/// Output-bit masks (bit 23 first) over the 24-bit input, transcribed from
/// the PRBS_P09 case of `pn1fn`.
pub const P09_MASKS: [u32; 24] = [
    0x110, 0x088, 0x044, 0x022, 0x011, 0x118, 0x08C, 0x046, 0x023, 0x101, 0x190, 0x0C8, 0x064, 0x032,
    0x019, 0x11C, 0x08E, 0x047, 0x133, 0x189, 0x1D4, 0x0EA, 0x075, 0x12A,
];

/// Same for PRBS_P11.
pub const P11_MASKS: [u32; 24] = [
    0x500, 0x280, 0x140, 0x0A0, 0x050, 0x028, 0x014, 0x00A, 0x005, 0x502, 0x281, 0x440, 0x220, 0x110,
    0x088, 0x044, 0x022, 0x011, 0x508, 0x284, 0x142, 0x0A1, 0x550, 0x2A8,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prbs {
    P09,
    P11,
}

pub fn pn1fn(sel: Prbs, din: u32) -> u32 {
    let masks = match sel {
        Prbs::P09 => &P09_MASKS,
        Prbs::P11 => &P11_MASKS,
    };
    let mut out = 0u32;
    for (k, m) in masks.iter().enumerate() {
        let bit = (din & m).count_ones() & 1;
        out |= bit << (23 - k);
    }
    out
}

/// 12-bit samples as the DAC emits them after a SYNC: seq = 0xFFFFFF, then
/// per pair: seq[23:12], seq[11:0], seq = pn1fn(seq).
pub fn dac_stream(sel: Prbs, n: usize) -> Vec<u32> {
    let mut seq = 0xFF_FFFFu32;
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        out.push((seq >> 12) & 0xFFF);
        if out.len() < n {
            out.push(seq & 0xFFF);
        }
        seq = pn1fn(sel, seq);
    }
    out
}

#[derive(Debug, Clone)]
pub struct Pn1Result {
    pub pairs_checked: u64,
    pub errors: u64,
    pub alignment: usize,
    pub zero_pairs: u64,
}

/// Self-synchronising check of one 12-bit channel stream: pairs
/// `{s[2k], s[2k+1]}` form 24-bit words that must satisfy
/// `d[k+1] == pn1fn(d[k])` (exactly the monitor's pn1 path). Both pair
/// alignments are tried; the better one is reported.
pub fn check_stream(samples12: &[u32], sel: Prbs) -> Pn1Result {
    let mut best: Option<Pn1Result> = None;
    for align in 0..2 {
        let words: Vec<u32> = samples12[align.min(samples12.len())..]
            .chunks_exact(2)
            .map(|c| ((c[0] & 0xFFF) << 12) | (c[1] & 0xFFF))
            .collect();
        let mut errors = 0u64;
        let mut zeros = 0u64;
        for w in words.windows(2) {
            if w[0] == 0 {
                zeros += 1;
            }
            if w[1] != pn1fn(sel, w[0]) || w[0] == 0 {
                errors += 1;
            }
        }
        let r = Pn1Result {
            pairs_checked: words.len().saturating_sub(1) as u64,
            errors,
            alignment: align,
            zero_pairs: zeros,
        };
        if best.as_ref().map(|b| r.errors < b.errors).unwrap_or(true) {
            best = Some(r);
        }
    }
    best.unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cross-checks the transcribed masks against the Verilog source when
    /// the adi-hdl submodule is checked out.
    #[test]
    fn masks_match_verilog() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../maia-hdl/adi-hdl/library/axi_ad9361/axi_ad9361_rx_pnmon.v");
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(_) => return, // submodule not present
        };
        for (case, masks) in [("PRBS_P09: begin", &P09_MASKS), ("PRBS_P11: begin", &P11_MASKS)] {
            let start = text.find(case).expect("case present");
            let mut got = [0u32; 24];
            for line in text[start + case.len()..].lines() {
                let line = line.trim();
                if line == "end" {
                    break;
                }
                if !line.starts_with("dout[") {
                    continue;
                }
                let k: usize = line[5..line.find(']').unwrap()].trim().parse().unwrap();
                let rhs = &line[line.find('=').unwrap() + 1..];
                let mut m = 0u32;
                for part in rhs.split('^') {
                    let part = part.trim().trim_end_matches(';');
                    let b: u32 = part[4..part.find(']').unwrap()].trim().parse().unwrap();
                    m ^= 1 << b;
                }
                got[23 - k] = m;
            }
            assert_eq!(&got, masks, "{case}");
        }
    }

    #[test]
    fn loopback_stream_checks_clean_and_detects_errors() {
        for sel in [Prbs::P09, Prbs::P11] {
            let mut s = dac_stream(sel, 4000);
            let r = check_stream(&s, sel);
            assert_eq!(r.errors, 0, "{sel:?}");
            assert!(r.pairs_checked > 1000);
            // Misaligned start (drop one sample) still locks.
            let r2 = check_stream(&s[1..], sel);
            assert_eq!(r2.errors, 0);
            s[1001] ^= 0x40;
            let r3 = check_stream(&s, sel);
            assert!(r3.errors >= 1 && r3.errors <= 2, "{}", r3.errors);
        }
    }
}
