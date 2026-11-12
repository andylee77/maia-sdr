//! The open-protocol data bands (315, 345, 433.92 and 902 to 928 MHz; `doc/DATA_MODE.md`): what
//! data mode makes of the radio's window. A channel around a hot spot is cut from the window's
//! raw IQ in software (`channel`), and the bursts in it are found (`bursts`): when each began
//! and ended, where it was and how strong. A burst is then sliced into its pulses (`pulses`)
//! and each decoder tries them: so far one, `oregon_v1`. A decoder is written from its
//! protocol's facts, never from another program's code.

pub mod bursts;
pub mod channel;
pub mod oregon_v1;
pub mod pulses;

use rustfft::num_complex::Complex32;
use serde::{Deserialize, Serialize};

/// What a device said in one burst, as every decoder gives it. A field a device does not send is
/// none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    /// The decoder's name for the device.
    pub model: String,
    /// `weather`, so far: what kind of device it is.
    pub kind: String,
    /// The device's id as its message gives it, and its channel (a switch on the device).
    pub id: u32,
    pub channel: Option<u8>,
    pub temperature_c: Option<f32>,
    pub battery_low: Option<bool>,
    /// How many times the burst held the message.
    pub repeats: u8,
}

impl Reading {
    /// The device it is of: `oregon_scientific_v1.3.1`.
    pub fn device_key(&self) -> String {
        let model: String = self.model.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
        match self.channel {
            Some(channel) => format!("{model}.{}.{channel}", self.id),
            None => format!("{model}.{}", self.id),
        }
    }
}

/// What the burst in `iq` says (the channel's samples, a burst with some of what is before and
/// after it; its centre `offset_hz` from the channel's): the first decoder that reads it.
pub fn read(iq: &[Complex32], offset_hz: f32) -> Option<Reading> {
    let on = pulses::sliced(iq, offset_hz);
    oregon_v1::read(&on, pulses::SAMPLE_US)
}

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

    #[test]
    fn a_reading_s_device_is_its_model_id_and_channel() {
        let r = Reading { model: oregon_v1::MODEL.into(), kind: "weather".into(), id: 3, channel: Some(1), temperature_c: Some(14.5), battery_low: Some(false), repeats: 2 };
        assert_eq!(r.device_key(), "oregon_scientific_v1.3.1");
        assert_eq!(Reading { channel: None, ..r }.device_key(), "oregon_scientific_v1.3");
    }

    /// A burst found in a raw capture: when it began and how long it was (s), where (Hz), the
    /// detector's own account of it, and what it says.
    struct Found {
        at: f64,
        length: f64,
        hz: f64,
        burst: Burst,
        reading: Option<Reading>,
    }

    /// The bursts `spot_hz` ± 98 kHz holds in a raw capture of a window, each read as the
    /// scanner reads it (with 20 ms either side), and the detector's counts.
    fn bursts_of(path: &std::path::Path, spot_hz: u64) -> (Vec<Found>, super::bursts::Counters) {
        let (lo, rate) = named_window(path.file_name().unwrap().to_str().unwrap()).expect("the capture's name gives its LO and rate");
        let mut channel = Channel::new(rate, spot_hz as f64 - lo as f64).unwrap();
        let mut detector = Detector::default();
        let mut file = std::fs::File::open(path).unwrap();
        let mut block = vec![0u8; 1 << 20];
        let (mut samples, mut all, mut found) = (Vec::new(), Vec::new(), Vec::new());
        loop {
            let n = file.read(&mut block).unwrap();
            if n == 0 {
                break;
            }
            samples.clear();
            channel.push(&block[..n / 4 * 4], &mut samples);
            all.extend_from_slice(&samples);
            detector.push(&samples, &mut found);
        }
        detector.end(&mut found);
        let s = |n: u64| n as f64 / f64::from(RATE_HZ);
        let margin = RATE_HZ as usize / 50;
        let found = found
            .into_iter()
            .map(|b| {
                let iq = &all[(b.start as usize).saturating_sub(margin)..(b.end as usize + margin).min(all.len())];
                Found { at: s(b.start), length: s(b.end - b.start), hz: spot_hz as f64 + f64::from(b.offset_hz), burst: b, reading: read(iq, b.offset_hz) }
            })
            .collect();
        (found, detector.counters())
    }

    fn captures(window: bool) -> Vec<std::path::PathBuf> {
        let Ok(dir) = std::env::var("DATA_CAPTURE_DIR") else { return Vec::new() };
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "cs16"))
            .filter(|p| named_window(&p.file_name().unwrap().to_string_lossy()).is_some_and(|(_, rate)| (rate > RATE_HZ) == window))
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no such .cs16 in {dir}");
        files
    }

    /// Unit A's first captures at 433.92 MHz (`DATA_CAPTURE_DIR`, doc/changes/109): 40 s each of
    /// a 2.5 MSPS window, under about 130 broadband clicks a second. What they hold is known
    /// from the look taken on the PC: four devices, one of them the Oregon Scientific sensor
    /// (its message twice in 392 ms, every 30 s, read there as 15.9 °C on channel 1). Printed
    /// with `--nocapture`.
    #[test]
    fn the_first_captures_hold_their_devices_and_no_click_is_a_burst() {
        // Two sensors keyed by pulse position, the Oregon Scientific sensor, and a weak one
        // that sends every 16 s.
        const DEVICES_HZ: [f64; 4] = [433_871_200.0, 433_880_150.0, 433_916_200.0, 433_923_900.0];
        const OREGON: usize = 1;
        for path in &captures(true) {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let (bursts, counters) = bursts_of(path, 433_920_000);
            println!("{name}: {} bursts, {counters:?}", bursts.len());
            for f in &bursts {
                let says = f.reading.as_ref().map_or(String::new(), |r| format!("  {} id {} channel {:?}: {:?} C, {} times", r.model, r.id, r.channel, r.temperature_c, r.repeats));
                println!("  {:7.3} s  {:6.1} ms  {:6.1} ms on  {:9.4} MHz  {:5.1} kHz wide  {:5.1} dB over{says}", f.at, f.length * 1e3, f.burst.on_ms, f.hz / 1e6, f.burst.width_hz / 1e3, f.burst.over_db);
            }
            let oregon_at: &[f64] = match name.split('_').next() {
                Some("look1") => &[7.992, 38.003],
                Some("look2") => &[6.936, 36.946],
                Some("look3") => &[17.880],
                _ => continue,
            };
            let device = |hz: f64| DEVICES_HZ.iter().position(|d| (d - hz).abs() < 1_500.0);
            assert!(bursts.iter().all(|f| device(f.hz).is_some()), "{name}: a burst that is none of the four devices");
            assert!(counters.clicks > 5_000, "{name}: {counters:?}");
            let oregon: Vec<_> = bursts.iter().filter(|f| device(f.hz) == Some(OREGON)).collect();
            assert_eq!(oregon.len(), oregon_at.len(), "{name}: the sensor's transmissions");
            for (f, at) in oregon.iter().zip(oregon_at) {
                assert!((f.at - at).abs() < 0.005 && (f.length - 0.392).abs() < 0.004 && f.burst.over_db > 27.0, "{name}: {:?}", f.burst);
                // Each of its bursts reads as the look read it, both times it is sent.
                let r = f.reading.as_ref().unwrap_or_else(|| panic!("{name}: the sensor's burst at {at} s is not read"));
                assert_eq!((r.model.as_str(), r.id, r.channel, r.battery_low, r.repeats), (oregon_v1::MODEL, 3, Some(1), Some(false), 2), "{name}");
                assert!((r.temperature_c.unwrap() - 15.9).abs() < 1e-4, "{name}: {r:?}");
            }
            // No other device's burst reads as the sensor.
            assert!(bursts.iter().filter(|f| device(f.hz) != Some(OREGON)).all(|f| f.reading.is_none()), "{name}: another device read as a sensor");
        }
    }

    /// The bursts unit A kept as captures (the `_250k.cs16` files of `DATA_CAPTURE_DIR`, each
    /// with its description): the sensor's read as the PC's look read them, 14.5 °C that night,
    /// and the others as nothing.
    #[test]
    fn the_unit_s_captures_of_the_sensor_read_as_the_look_read_them() {
        for path in &captures(false) {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(path.with_extension("sigmf-meta")).expect("its description")).unwrap();
            let burst = &meta["annotations"][0]["fishball:burst"];
            let (hz, centre) = (burst["freq_hz"].as_f64().unwrap(), burst["centre_hz"].as_f64().unwrap());
            let iq: Vec<Complex32> = std::fs::read(path)
                .unwrap()
                .chunks_exact(4)
                .map(|s| Complex32::new(f32::from(i16::from_le_bytes([s[0], s[1]])), f32::from(i16::from_le_bytes([s[2], s[3]]))))
                .collect();
            let reading = read(&iq, (hz - centre) as f32);
            println!("{name}: {:.4} MHz, {} ms: {reading:?}", hz / 1e6, burst["length_ms"]);
            if (hz - 433_879_400.0).abs() < 1_500.0 && burst["length_ms"].as_u64().is_some_and(|ms| ms < 400) {
                let r = reading.unwrap_or_else(|| panic!("{name}: the sensor's capture is not read"));
                assert_eq!((r.id, r.channel, r.repeats), (3, Some(1), 2), "{name}");
                assert!((r.temperature_c.unwrap() - 14.5).abs() < 1e-4, "{name}: {r:?}");
            } else if (hz - 433_879_400.0).abs() > 5_000.0 {
                assert!(reading.is_none(), "{name}: another device read as a sensor");
            }
        }
    }
}
