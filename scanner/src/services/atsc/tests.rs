use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use super::sweep::Atsc;
use super::*;
use crate::hardware::ad9361::GainMode;
use crate::hardware::presets::DdcPreset;
use crate::protocol::atsc::tests::{add_flat, add_vsb, noise};
use crate::radio::lane::Lane;
use crate::radio::lease::{Lease, RadioLease};
use crate::radio::tuner::{RadioHw, Readback, Tuner};
use crate::services::events::EventLog;

/// On the air: (channel, ATSC 3.0, carrier to noise in dB, pilot offset in Hz).
const AIR: &[(u8, bool, f64, f64)] =
    &[(9, false, 30.0, 0.0), (18, true, 25.0, 0.0), (19, false, 35.0, 10_100.0), (20, false, 25.0, 0.0), (36, false, 15.0, 0.0)];
/// Clips the ADC whenever it is in the window.
const LOUD: u8 = 36;

/// A radio on that air: the spectrometer shows what falls in the window around the LO; each frame
/// is a million samples, a thousand of them clipped while `LOUD` is in the window.
#[derive(Default)]
struct Air {
    lo: AtomicU64,
    clips: AtomicU32,
    samples: AtomicU64,
    gains: Mutex<Vec<String>>,
}

/// Spectrometer words (exponent 0): the noise at -100 dB, inside the 47-bit mantissa.
fn words(p: &[f64]) -> Vec<u8> {
    p.iter().flat_map(|&x| ((x * 10f64.powf(4.8)) as u64).to_le_bytes()).collect()
}

impl RadioHw for Air {
    async fn set_lo(&self, hz: u64) -> Result<()> {
        self.lo.store(hz, Ordering::SeqCst);
        Ok(())
    }
    async fn set_rate(&self, _: u32, _: u32) -> Result<()> {
        Ok(())
    }
    async fn set_gain(&self, mode: GainMode, db: Option<f64>) -> Result<()> {
        self.gains.lock().unwrap().push(format!("{} {db:?}", mode.as_str()));
        Ok(())
    }
    async fn configure_control(&self, _: &'static DdcPreset, _: f64) -> Result<()> {
        Ok(())
    }
    async fn set_control_nco(&self, _: f64, _: u32) -> Result<()> {
        Ok(())
    }
    async fn configure_lanes(&self, _: &'static DdcPreset) -> Result<()> {
        Ok(())
    }
    async fn retune_lane(&self, _: Lane, _: f64, _: u32, _: bool) -> Result<()> {
        Ok(())
    }
    async fn pause_lane(&self, _: Lane) -> Result<()> {
        Ok(())
    }
    async fn readback(&self, _: u32) -> Readback {
        Readback {
            gain_db: Some(40.0),
            adc_clips: Some(self.clips.load(Ordering::SeqCst)),
            sample_count: Some(self.samples.load(Ordering::SeqCst)),
            ..Default::default()
        }
    }
    async fn spectrum(&self) -> Option<Vec<u8>> {
        let lo = self.lo.load(Ordering::SeqCst) as f64;
        self.samples.fetch_add(1_000_000, Ordering::SeqCst);
        let mut p = noise();
        for &(n, flat, cn, shift) in AIR {
            let c = Channel::get(n).unwrap();
            if c.low_hz as f64 > lo + 8e6 || (c.high_hz() as f64) < lo - 8e6 {
                continue;
            }
            if flat {
                add_flat(&mut p, lo, c, cn);
            } else {
                add_vsb(&mut p, lo, c, cn, shift);
            }
            if n == LOUD {
                self.clips.fetch_add(1_000, Ordering::SeqCst);
            }
        }
        Some(words(&p))
    }
}

async fn finished(atsc: &Atsc) -> AtscScan {
    while atsc.state().running() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    atsc.state()
}

#[tokio::test(start_paused = true)]
async fn a_scan_reads_each_channel_and_says_what_it_holds() {
    let atsc = Arc::new(Atsc::default());
    let tuner = Arc::new(Tuner::new(Air::default(), 0.0));
    let log = Arc::new(EventLog::default());
    let req = AtscRequest { channels: vec![36, 9, 18, 19, 20, 25], ..Default::default() };
    let refused = atsc.start(req.clone(), tuner.clone(), log.clone()).unwrap_err();
    assert!(refused.to_string().contains("not in ATSC mode"), "{refused}");
    let lease = RadioLease::default();
    atsc.enter(lease.take(Lease::Atsc).unwrap(), Some("clay".into())).await;
    let id = atsc.start(req.clone(), tuner.clone(), log.clone()).unwrap();
    assert!(atsc.start(req, tuner.clone(), log.clone()).is_err(), "one scan at a time");
    let s = finished(&atsc).await;
    // 9 alone, 18 with 19, then 20, 25 and 36 alone.
    assert_eq!((s.id, s.state, s.steps), (id, "done", 5));
    let kinds: Vec<(u8, Kind)> = s.found.iter().map(|c| (c.number, c.kind)).collect();
    assert_eq!(kinds, [(9, Kind::Vsb), (18, Kind::NoPilot), (19, Kind::Vsb), (20, Kind::Vsb), (25, Kind::Vacant), (36, Kind::Vsb)]);
    let c19 = &s.found[2];
    assert!((c19.pilot_offset_hz.unwrap() - 10_100).abs() < 300, "{c19:?}");
    assert!((c19.level_db - 35.0).abs() < 1.0, "{c19:?}");
    assert!((s.found[3].level_db - 25.0).abs() < 1.0, "beside a strong neighbour: {:?}", s.found[3]);
    assert!(s.found.iter().all(|c| c.gain_db == Some(40.0) && c.power_dbm.is_some()));
    for c in &s.found {
        let want = if c.number == LOUD { 1_000.0 } else { 0.0 };
        assert!((c.clips_ppm.unwrap() - want).abs() < 1e-6, "{c:?}");
    }
    assert_eq!(tuner.hw().gains.lock().unwrap().as_slice(), ["slow_attack None"], "the AGC unless a gain is asked");
    let last = log.since(0, 10, false).last().unwrap().text.clone();
    assert!(last.contains("done on 6 channels: 4 8-VSB, 1 without an 8-VSB pilot"), "{last}");
    // RF 19's stretch of its window: the channel and 0.5 MHz either side, its pilot the peak.
    let sp = atsc.channel_spectrum(19).unwrap();
    assert_eq!((sp.lo_hz, sp.low_hz, sp.pilot_hz), (500_500_000, 500_000_000, 500_309_441));
    assert!((sp.start_hz - 499_500_000.0).abs() <= sp.bin_hz && (sp.db.len() as f64 * sp.bin_hz - 7e6).abs() <= 2.0 * sp.bin_hz);
    let peak = sp.db.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
    let peak_hz = sp.start_hz + peak as f64 * sp.bin_hz;
    assert!((peak_hz - (sp.pilot_hz as f64 + 10_100.0)).abs() < sp.bin_hz, "{peak_hz}");
    assert!(atsc.channel_spectrum(30).is_none(), "not read");
    // Leaving hands the radio back, with the site to bring back.
    assert_eq!(atsc.leave().await.as_deref(), Some("clay"));
    assert!(lease.is_normal());
}

#[tokio::test(start_paused = true)]
async fn leaving_atsc_mode_stops_a_scan_first() {
    let atsc = Arc::new(Atsc::default());
    let tuner = Arc::new(Tuner::new(Air::default(), 0.0));
    let log = Arc::new(EventLog::default());
    let lease = RadioLease::default();
    atsc.enter(lease.take(Lease::Atsc).unwrap(), None).await;
    let req = AtscRequest { gain_db: Some(30), ..Default::default() };
    atsc.start(req.clone(), tuner.clone(), log.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(atsc.state().running());
    assert_eq!(atsc.leave().await, None);
    let s = atsc.state();
    assert_eq!(s.state, "cancelled");
    assert!(s.found.len() < req.channels().len(), "{}", s.found.len());
    assert!(lease.is_normal());
    assert!(atsc.start(req, tuner.clone(), log).is_err(), "no scan outside ATSC mode");
    assert_eq!(tuner.hw().gains.lock().unwrap().as_slice(), ["manual Some(30.0)"]);
}

#[test]
fn a_request_names_reachable_channels_and_sane_settings() {
    assert!(AtscRequest::default().check().is_ok());
    assert_eq!(AtscRequest::default().channels(), (4..=36).collect::<Vec<u8>>());
    for bad in [
        AtscRequest { channels: vec![3], ..Default::default() },
        AtscRequest { channels: vec![37], ..Default::default() },
        AtscRequest { frames: 0, ..Default::default() },
        AtscRequest { gain_db: Some(80), ..Default::default() },
    ] {
        assert!(bad.check().is_err(), "{bad:?}");
    }
    let o = options();
    assert_eq!(o.channels.len(), 35);
    assert_eq!(o.channels.iter().filter(|c| !c.reachable).map(|c| c.number).collect::<Vec<u8>>(), [2, 3]);
}
