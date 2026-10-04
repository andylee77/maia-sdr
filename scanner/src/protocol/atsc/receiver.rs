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

#[cfg(test)]
mod tests {
    use super::*;

    /// Interleaved little-endian i16 I, Q.
    fn read_cs16(path: &std::path::Path) -> Vec<Complex32> {
        let b = std::fs::read(path).unwrap();
        b.chunks_exact(4)
            .map(|c| Complex32::new(f32::from(i16::from_le_bytes([c[0], c[1]])), f32::from(i16::from_le_bytes([c[2], c[3]]))))
            .collect()
    }

    /// Offline: unit A's captures (`ATSC_CAPTURE_DIR` holding `atsc_rf<N>.cs16`: 10 MSPS, the
    /// channel's centre at DC, unit A's crystal -0.6967 ppm), against Andy's HDHomeRun.
    #[test]
    fn captured_stations_name_themselves() {
        let Ok(dir) = std::env::var("ATSC_CAPTURE_DIR") else {
            eprintln!("ATSC_CAPTURE_DIR not set: skipped");
            return;
        };
        let fs = 10e6 * (1.0 - 0.6967e-6);
        // RF channel, its TSID and names as the HDHomeRun has them; the hard ones (weak, or
        // distorted on unit A's antenna) are printed, not required.
        let stations: [(u32, u16, &[&str]); 6] = [
            (20, 601, &["WJXT-HD", "WCWJ-HD"]),
            (19, 605, &["WJAXHD", "WJAXTN"]),
            (21, 607, &[]),
            (17, 10051, &[]),
            (23, 9469, &[]),
            (14, 603, &[]),
        ];
        for (rf, tsid, names) in stations {
            let path = std::path::Path::new(&dir).join(format!("atsc_rf{rf}.cs16"));
            let t = std::time::Instant::now();
            let Some(id) = identify(&read_cs16(&path), fs, 0.0) else {
                eprintln!("RF {rf}: no syncs");
                assert!(names.is_empty(), "RF {rf}");
                continue;
            };
            eprintln!(
                "RF {rf}: {:.1} s, MER {:.1} dB, pilot {:+.1} Hz, clock {:+.2} ppm, {} fields, {} packets ({} corrected, {} failed), TSID {:?}: {:?}",
                t.elapsed().as_secs_f32(),
                id.mer_db,
                id.pilot_offset_hz,
                id.clock_ppm,
                id.fields,
                id.packets,
                id.corrected,
                id.failed,
                id.psip.tsid,
                id.psip.channels.iter().map(|c| format!("{}.{} {}", c.major, c.minor, c.short_name)).collect::<Vec<_>>()
            );
            if names.is_empty() {
                continue;
            }
            assert_eq!(id.psip.tsid, Some(tsid), "RF {rf}");
            for n in names {
                assert!(id.psip.channels.iter().any(|c| c.short_name == *n), "RF {rf}: no {n}");
            }
        }
    }
}
