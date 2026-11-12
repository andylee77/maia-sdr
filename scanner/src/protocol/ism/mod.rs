//! The open-protocol data bands (315, 345, 433.92 and 902 to 928 MHz; `doc/DATA_MODE.md`): what
//! data mode makes of the radio's window. A channel around a hot spot is cut from the window's
//! raw IQ in software (`channel`), and the bursts in it are found (`bursts`): when each began
//! and ended, where it was and how strong. A burst's envelope is then read as pulses (`pulses`),
//! through a filter as narrow as the keyings allow, by each decoder in turn: `oregon_v1`, and
//! `distance`, a table of the devices keyed by pulse distance. A decoder is written from its
//! protocol's facts, never from another program's code.

pub mod bursts;
pub mod channel;
pub mod distance;
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
    /// Relative humidity, per cent.
    #[serde(default)]
    pub humidity_pct: Option<u8>,
    pub battery_low: Option<bool>,
    /// How many times the burst held the message.
    pub repeats: u8,
}

impl Reading {
    /// The device it is of: `oregon_scientific_v1.3.1`, or `acurite_606tx.181` for one that has
    /// no channel.
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
    let on = pulses::slice(&pulses::envelope(iq, offset_hz, pulses::HALF_HZ), pulses::SMOOTH);
    oregon_v1::read(&on, pulses::SAMPLE_US).or_else(|| distance::read(&on, pulses::SAMPLE_US))
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
        let r = Reading { model: oregon_v1::MODEL.into(), kind: "weather".into(), id: 3, channel: Some(1), temperature_c: Some(14.5), humidity_pct: None, battery_low: Some(false), repeats: 2 };
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
    /// from the looks taken on the PC: four devices. The Oregon Scientific sensor (its message
    /// twice in 392 ms, every 30 s, read there as 15.9 °C on channel 1), a Prologue sensor
    /// 8.4 kHz under it (17.1 °C, 75 then 76 %), an AcuRite 606TX (21.2 then 21.1 °C), and a weak
    /// one that sends every 16 s and that nothing reads. Printed with `--nocapture`.
    #[test]
    fn the_first_captures_hold_their_devices_and_no_click_is_a_burst() {
        const DEVICES_HZ: [f64; 4] = [433_871_200.0, 433_880_150.0, 433_916_200.0, 433_923_900.0];
        const PROLOGUE: usize = 0;
        const OREGON: usize = 1;
        const ACURITE: usize = 2;
        for path in &captures(true) {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let (bursts, counters) = bursts_of(path, 433_920_000);
            println!("{name}: {} bursts, {counters:?}", bursts.len());
            for f in &bursts {
                let says = f.reading.as_ref().map_or(String::new(), |r| format!("  {} id {} channel {:?}: {:?} C, {:?} %, {} times", r.model, r.id, r.channel, r.temperature_c, r.humidity_pct, r.repeats));
                println!("  {:7.3} s  {:6.1} ms  {:6.1} ms on  {:9.4} MHz  {:5.1} kHz wide  {:5.1} dB over{says}", f.at, f.length * 1e3, f.burst.on_ms, f.hz / 1e6, f.burst.width_hz / 1e3, f.burst.over_db);
            }
            // When the Oregon sensor sent, and how many bursts of the other two are whole.
            let (oregon_at, prologues, acurites): (&[f64], usize, usize) = match name.split('_').next() {
                Some("look1") => (&[7.992, 38.003], 1, 1),
                Some("look2") => (&[6.936, 36.946], 1, 1),
                Some("look3") => (&[17.880], 1, 2),
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
            // The two keyed by pulse distance read as the looks read them, each time they sent.
            let read_as = |which: usize| bursts.iter().filter(move |f| device(f.hz) == Some(which)).filter_map(|f| f.reading.as_ref());
            assert_eq!((read_as(PROLOGUE).count(), read_as(ACURITE).count()), (prologues, acurites), "{name}");
            for r in read_as(PROLOGUE) {
                assert_eq!((r.model.as_str(), r.id, r.channel, r.battery_low), ("Prologue TH", 86, Some(2), Some(true)), "{name}");
                assert!((r.temperature_c.unwrap() - 17.1).abs() < 1e-4 && matches!(r.humidity_pct, Some(75 | 76)) && r.repeats >= 3, "{name}: {r:?}");
            }
            for r in read_as(ACURITE) {
                assert_eq!((r.model.as_str(), r.id, r.channel, r.humidity_pct, r.battery_low, r.repeats), ("AcuRite 606TX", 181, None, None, Some(false), 6), "{name}");
                assert!((21.1 - 1e-4..=21.2 + 1e-4).contains(&r.temperature_c.unwrap()), "{name}: {r:?}");
            }
            // The weak one reads as nothing.
            assert!(bursts.iter().filter(|f| device(f.hz).is_some_and(|d| d > ACURITE)).all(|f| f.reading.is_none()), "{name}: the weak device read as a sensor");
        }
    }

    /// The bursts unit A kept as captures (the `_250k.cs16` files of `DATA_CAPTURE_DIR`, each
    /// with its description), read as the PC's look reads them. Those of 04 h UTC: the Oregon
    /// Scientific sensor's 14.5 °C, the Prologue sensor's 15.7 °C and 84 or 85 %, the AcuRite's
    /// 19.3 °C. Those of 13 h, when the band's floor was 2 to 8 dB higher: 14.2 °C, 15.4 or
    /// 15.5 °C and 89 %, 14.5 °C. The weak device's read as nothing.
    #[test]
    fn the_unit_s_captures_read_as_the_look_read_them() {
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
            let near = |device_hz: f64| (hz - device_hz).abs() < 1_500.0;
            let alone = burst["length_ms"].as_u64().is_some_and(|ms| ms < 900);
            let night = name.starts_with("20261112T04");
            let is = |r: &Reading, temperatures: &[f32]| temperatures.iter().any(|t| (r.temperature_c.unwrap() - t).abs() < 1e-4);
            if near(433_879_400.0) && burst["length_ms"].as_u64().is_some_and(|ms| ms < 400) {
                let r = reading.unwrap_or_else(|| panic!("{name}: the sensor's capture is not read"));
                assert_eq!((r.model.as_str(), r.id, r.channel, r.repeats), (oregon_v1::MODEL, 3, Some(1), 2), "{name}");
                assert!(is(&r, if night { &[14.5] } else { &[14.2] }), "{name}: {r:?}");
            } else if near(433_871_200.0) && alone {
                let r = reading.unwrap_or_else(|| panic!("{name}: the Prologue sensor's capture is not read"));
                assert_eq!((r.model.as_str(), r.id, r.channel, r.battery_low), ("Prologue TH", 86, Some(2), Some(true)), "{name}");
                assert!(r.repeats >= 3 && if night { is(&r, &[15.7]) && matches!(r.humidity_pct, Some(84 | 85)) } else { is(&r, &[15.4, 15.5]) && r.humidity_pct == Some(89) }, "{name}: {r:?}");
            } else if near(433_916_300.0) || near(433_917_200.0) {
                let r = reading.unwrap_or_else(|| panic!("{name}: the AcuRite's capture is not read"));
                assert_eq!((r.model.as_str(), r.id, r.battery_low, r.repeats), ("AcuRite 606TX", 181, Some(false), 6), "{name}");
                assert!(is(&r, if night { &[19.3] } else { &[14.5] }), "{name}: {r:?}");
            } else if near(433_923_800.0) {
                assert!(reading.is_none(), "{name}: the weak device read as a sensor");
            }
        }
    }
}
