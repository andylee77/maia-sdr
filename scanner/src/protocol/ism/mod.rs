//! The open-protocol data bands (315, 345, 433.92 and 902 to 928 MHz; `doc/DATA_MODE.md`): what
//! data mode makes of the radio's window. A channel around a hot spot is cut from the window's
//! raw IQ in software (`channel`), and the bursts in it are found (`bursts`): when each began
//! and ended, where it was and how strong. No device is decoded here yet.

pub mod bursts;
pub mod channel;

/// The LO (Hz) and sample rate (SPS) a raw capture of a window says in its file's name, as the
/// tools of these bands write them: `..._434.42M_2500k.cs16`.
pub fn named_window(file_name: &str) -> Option<(u64, u32)> {
    let stem = file_name.rsplit_once('.').map_or(file_name, |(stem, _)| stem);
    let number = |token: &str, unit: char| token.strip_suffix(unit).and_then(|n| n.parse::<f64>().ok()).filter(|v| *v > 0.0);
    let lo = stem.split('_').find_map(|t| number(t, 'M'))?;
    let rate = stem.split('_').find_map(|t| number(t, 'k'))?;
    Some(((lo * 1e6).round() as u64, (rate * 1e3).round() as u32))
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::bursts::{Burst, Detector};
    use super::channel::{Channel, RATE_HZ};
    use super::*;

    #[test]
    fn a_capture_s_name_gives_its_window() {
        assert_eq!(named_window("look1_434.42M_2500k.cs16"), Some((434_420_000, 2_500_000)));
        assert_eq!(named_window("20261112T022554Z_0007_433.92M_250k.cs16"), Some((433_920_000, 250_000)));
        assert_eq!(named_window("g001_315M_1000k.cu8"), Some((315_000_000, 1_000_000)));
        assert_eq!(named_window("look1.cs16"), None);
        assert_eq!(named_window("capture_250k.cs16"), None);
    }

    /// The bursts `spot_hz` ± 98 kHz holds in a raw capture, with their times in seconds and
    /// frequencies in Hz, and the detector's counts.
    fn bursts_of(path: &std::path::Path, spot_hz: u64) -> (Vec<(f64, f64, f64, Burst)>, super::bursts::Counters) {
        let (lo, rate) = named_window(path.file_name().unwrap().to_str().unwrap()).expect("the capture's name gives its LO and rate");
        let mut channel = Channel::new(rate, spot_hz as f64 - lo as f64).unwrap();
        let mut detector = Detector::default();
        let mut file = std::fs::File::open(path).unwrap();
        let mut block = vec![0u8; 1 << 20];
        let (mut samples, mut found) = (Vec::new(), Vec::new());
        loop {
            let n = file.read(&mut block).unwrap();
            if n == 0 {
                break;
            }
            samples.clear();
            channel.push(&block[..n / 4 * 4], &mut samples);
            detector.push(&samples, &mut found);
        }
        detector.end(&mut found);
        let s = |n: u64| n as f64 / f64::from(RATE_HZ);
        (found.into_iter().map(|b| (s(b.start), s(b.end - b.start), spot_hz as f64 + f64::from(b.offset_hz), b)).collect(), detector.counters())
    }

    /// Unit A's first captures at 433.92 MHz (`DATA_CAPTURE_DIR`, doc/changes/109): 40 s each of
    /// a 2.5 MSPS window, under about 130 broadband clicks a second. What they hold is known
    /// from the look taken on the PC: four devices, one of them the Oregon Scientific sensor
    /// (its message twice in 392 ms, every 30 s, decoded there). Printed with `--nocapture`.
    #[test]
    fn the_first_captures_hold_their_devices_and_no_click_is_a_burst() {
        let Ok(dir) = std::env::var("DATA_CAPTURE_DIR") else { return };
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "cs16")).collect();
        files.sort();
        assert!(!files.is_empty(), "no .cs16 in {dir}");
        // Two sensors keyed by pulse position, the Oregon Scientific sensor, and a weak one
        // that sends every 16 s.
        const DEVICES_HZ: [f64; 4] = [433_871_200.0, 433_880_150.0, 433_916_200.0, 433_923_900.0];
        const OREGON: usize = 1;
        for path in &files {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let (bursts, counters) = bursts_of(path, 433_920_000);
            println!("{name}: {} bursts, {counters:?}", bursts.len());
            for (t, length, hz, b) in &bursts {
                println!("  {t:7.3} s  {:6.1} ms  {:6.1} ms on  {:9.4} MHz  {:5.1} kHz wide  {:5.1} dB over", length * 1e3, b.on_ms, hz / 1e6, b.width_hz / 1e3, b.over_db);
            }
            let oregon_at: &[f64] = match name.split('_').next() {
                Some("look1") => &[7.992, 38.003],
                Some("look2") => &[6.936, 36.946],
                Some("look3") => &[17.880],
                _ => continue,
            };
            let device = |hz: f64| DEVICES_HZ.iter().position(|d| (d - hz).abs() < 1_500.0);
            assert!(bursts.iter().all(|b| device(b.2).is_some()), "{name}: a burst that is none of the four devices");
            assert!(counters.clicks > 5_000, "{name}: {counters:?}");
            let oregon: Vec<_> = bursts.iter().filter(|b| device(b.2) == Some(OREGON)).collect();
            assert_eq!(oregon.len(), oregon_at.len(), "{name}: the sensor's transmissions");
            for (b, at) in oregon.iter().zip(oregon_at) {
                assert!((b.0 - at).abs() < 0.005 && (b.1 - 0.392).abs() < 0.004 && b.3.over_db > 27.0, "{name}: {b:?}");
            }
        }
    }
}
