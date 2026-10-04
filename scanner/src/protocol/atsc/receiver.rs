//! A channel's IQ to what its station says: demodulated, decoded to transport stream packets,
//! its PSIP read.

use rustfft::num_complex::Complex32;
use serde::Serialize;

use super::demod::demodulate;
use super::fec::{self, rs, FecStats};
use super::psip::Psip;
use super::ts::Sections;

#[derive(Debug, Clone, Serialize)]
pub struct Identified {
    pub psip: Psip,
    /// Modulation error ratio of the equalized symbols, dB.
    pub mer_db: f32,
    pub pilot_offset_hz: f64,
    /// The symbol clock against the sample clock as given, ppm.
    pub clock_ppm: f64,
    pub fields: usize,
    pub packets: usize,
    /// Packets Reed-Solomon corrected, and those it could not.
    pub corrected: usize,
    pub failed: usize,
}

/// Identify the station on a channel: `iq` at `sample_rate_hz` (the true rate), the channel's
/// centre `centre_offset_hz` from DC. `None` when the signal gives no syncs.
pub fn identify(iq: &[Complex32], sample_rate_hz: f64, centre_offset_hz: f64) -> Option<Identified> {
    let d = demodulate(iq, sample_rate_hz, centre_offset_hz)?;
    let (packets, FecStats { fields, packets: n, corrected, failed }) = fec::decode(&d.segments, d.first_field_sync?);
    let mut sections = Sections::default();
    let mut psip = Psip::default();
    for p in packets.iter().filter(|p| p.outcome != rs::Outcome::Failed) {
        for (pid, s) in sections.push(&p.bytes, &Psip::pids()) {
            psip.section(pid, &s);
        }
    }
    Some(Identified {
        psip,
        mer_db: d.mer_db,
        pilot_offset_hz: d.pilot_offset_hz,
        clock_ppm: d.clock_ppm,
        fields,
        packets: n,
        corrected,
        failed,
    })
}

/// An 8-VSB signal for tests: `packets` (312 a field) through the FEC, segment and field syncs
/// put in, the pilot added, then the VSB pulse in the channel-centred signal (symbol k as
/// a_k (−j)^k through a real RRC at RS/2) sampled at `fs`, with uniform noise of `noise` on I
/// and Q.
#[cfg(test)]
pub fn modulate(packets: &[[u8; 188]], fs: f64, noise: f32) -> Vec<Complex32> {
    use super::demod::SYMBOL_RATE;
    use super::vsb::{field_sync, FIELD_SEGMENTS, PILOT, SEGMENT_SYNC};

    let mut segs = fec::encode(packets);
    for (i, seg) in segs.iter_mut().enumerate() {
        if i % FIELD_SEGMENTS == 0 {
            let known = field_sync((i / FIELD_SEGMENTS) % 2 == 1);
            seg[..known.len()].copy_from_slice(&known);
        } else {
            seg[..4].copy_from_slice(&SEGMENT_SYNC);
        }
    }
    let syms: Vec<f32> = segs.iter().flatten().map(|&s| s + PILOT).collect();
    // The RRC at RS/2 over ±24 symbols of RS, at 1/256 of a symbol.
    const STEPS: usize = 256;
    let span = 24isize;
    let table: Vec<f32> = (0..=span as usize * STEPS).map(|j| super::demod::rrc(j as f64 / STEPS as f64 / 2.0) as f32).collect();
    let n = (syms.len() as f64 / SYMBOL_RATE * fs) as usize;
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut uniform = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    let rot = [Complex32::new(1.0, 0.0), Complex32::new(0.0, -1.0), Complex32::new(-1.0, 0.0), Complex32::new(0.0, 1.0)];
    (0..n)
        .map(|i| {
            let t = i as f64 / fs * SYMBOL_RATE;
            let k0 = t.floor() as isize;
            let mut acc = Complex32::new(0.0, 0.0);
            for k in (k0 - span + 1).max(0)..(k0 + span).min(syms.len() as isize) {
                let p = table[((t - k as f64).abs() * STEPS as f64).round() as usize];
                acc += rot[(k % 4) as usize] * (syms[k as usize] * p);
            }
            acc + Complex32::new(noise * uniform(), noise * uniform())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::atsc::ts::tests::{packets as ts_packets, section};

    /// Each field: a PAT (TSID 601) and a TVCT naming 4.1 WJXT-HD (stations repeat them), then
    /// null packets.
    fn station_packets(fields: usize) -> Vec<[u8; 188]> {
        let mut tables = ts_packets(0x0000, &section(0x00, &[0x02, 0x59, 0xC1, 0, 0, 0, 7, 0xE0, 0x31]));
        let mut ch = Vec::new();
        let mut name: Vec<u16> = "WJXT-HD".encode_utf16().collect();
        name.resize(7, 0);
        for u in name {
            ch.extend_from_slice(&u.to_be_bytes());
        }
        ch.extend_from_slice(&[0xF0, 4 << 2, 1, 0x04, 0, 0, 0, 0, 0x02, 0x59, 0, 7, 0x0D, 0xC2, 0, 5, 0xFC, 0]);
        let mut body = vec![0x02, 0x59, 0xC1, 0, 0, 0, 1];
        body.extend(ch);
        body.extend_from_slice(&[0xFC, 0]);
        tables.extend(ts_packets(0x1FFB, &section(0xC8, &body)));
        let mut null = [0xFFu8; 188];
        null[..4].copy_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
        let mut out = Vec::with_capacity(fields * 312);
        for _ in 0..fields {
            out.extend_from_slice(&tables);
            out.resize(out.len() + 312 - tables.len(), null);
        }
        out
    }

    #[test]
    fn a_modulated_station_names_itself() {
        // Four fields at 10 MSPS, noise of ±0.3 on I and Q.
        let iq = modulate(&station_packets(4), 10e6, 0.6);
        let id = identify(&iq, 10e6, 0.0).expect("syncs");
        assert!(id.mer_db > 20.0, "MER {}", id.mer_db);
        assert_eq!((id.failed, id.psip.tsid), (0, Some(601)), "{id:?}");
        let c = &id.psip.channels;
        assert_eq!((c.len(), c[0].major, c[0].minor, c[0].short_name.as_str()), (1, 4, 1, "WJXT-HD"));
    }

    #[test]
    fn a_station_off_centre_and_off_clock_still_decodes() {
        // The channel 40 kHz off DC, the sample clock 2 ppm fast.
        let fs = 10e6;
        let iq: Vec<Complex32> = modulate(&station_packets(4), fs, 0.6)
            .into_iter()
            .enumerate()
            .map(|(k, s)| s * Complex32::from_polar(1.0, (2.0 * std::f64::consts::PI * 40e3 * k as f64 / fs) as f32))
            .collect();
        let id = identify(&iq, fs * (1.0 + 2e-6), 40e3).expect("syncs");
        assert_eq!(id.psip.tsid, Some(601), "{id:?}");
    }

    /// Interleaved little-endian i16 I, Q.
    fn read_cs16(path: &std::path::Path) -> Vec<Complex32> {
        let b = std::fs::read(path).unwrap();
        b.chunks_exact(4)
            .map(|c| Complex32::new(f32::from(i16::from_le_bytes([c[0], c[1]])), f32::from(i16::from_le_bytes([c[2], c[3]]))))
            .collect()
    }

    /// Offline: unit A's captures (`ATSC_CAPTURE_DIR` holding `atsc_rf<N>.cs16` through its UHF
    /// omni and `atsc_rf<N>_dir.cs16` through a VHF/UHF directional antenna: 10 MSPS, the
    /// channel's centre at DC, unit A's crystal -0.6967 ppm), against Andy's HDHomeRun.
    #[test]
    fn captured_stations_name_themselves() {
        let Ok(dir) = std::env::var("ATSC_CAPTURE_DIR") else {
            eprintln!("ATSC_CAPTURE_DIR not set: skipped");
            return;
        };
        let fs = 10e6 * (1.0 - 0.6967e-6);
        // Capture, TSID and names as the HDHomeRun has them; none: printed only (too weak there,
        // or distorted on the omni).
        let captures: &[(&str, u16, &[&str])] = &[
            ("atsc_rf20", 601, &["WJXT-HD", "WCWJ-HD"]),
            ("atsc_rf19", 605, &["WJAXHD", "WJAXTN"]),
            ("atsc_rf21", 607, &["TBN HD"]),
            ("atsc_rf14_dir", 603, &["WFOXHD"]),
            ("atsc_rf17_dir", 10051, &["WJVF-LD"]),
            ("atsc_rf23_dir", 9469, &["WKBJ-LD"]),
            ("atsc_rf9_dir", 597, &["WJCT-HD", "DABL"]),
            ("atsc_rf13_dir", 599, &["WTLV-HD"]),
            ("atsc_rf20_dir", 601, &["WJXT-HD"]),
            ("atsc_rf11_dir", 0, &[]),
            ("atsc_rf10_dir", 657, &[]),
            ("atsc_rf14", 603, &[]),
            ("atsc_rf17", 10051, &[]),
            ("atsc_rf23", 9469, &[]),
            ("atsc_rf15_dir", 4083, &[]),
            ("atsc_rf22_dir", 813, &[]),
            ("atsc_rf36_dir", 587, &[]),
        ];
        for &(name, tsid, names) in captures {
            let path = std::path::Path::new(&dir).join(format!("{name}.cs16"));
            if !path.exists() {
                continue;
            }
            let t = std::time::Instant::now();
            let Some(id) = identify(&read_cs16(&path), fs, 0.0) else {
                eprintln!("{name}: no syncs");
                assert!(names.is_empty(), "{name}");
                continue;
            };
            eprintln!(
                "{name}: {:.1} s, MER {:.1} dB, pilot {:+.1} Hz, clock {:+.2} ppm, {} packets ({} corrected, {} failed), TSID {:?}: {:?}",
                t.elapsed().as_secs_f32(),
                id.mer_db,
                id.pilot_offset_hz,
                id.clock_ppm,
                id.packets,
                id.corrected,
                id.failed,
                id.psip.tsid,
                id.psip.channels.iter().map(|c| format!("{}.{} {}", c.major, c.minor, c.short_name)).collect::<Vec<_>>()
            );
            if names.is_empty() {
                continue;
            }
            assert_eq!(id.psip.tsid, Some(tsid), "{name}");
            for n in names {
                assert!(id.psip.channels.iter().any(|c| c.short_name == *n), "{name}: no {n}");
            }
        }
    }
}
