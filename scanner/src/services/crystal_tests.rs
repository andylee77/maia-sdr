use std::sync::Mutex as StdMutex;

use super::*;
use crate::hardware::ad9361::GainMode;
use crate::radio::lane::Lane;
use crate::hardware::presets::{find_preset, DdcPreset};
use crate::protocol::events::P25Identity;
use crate::radio::tuner::{lo_shift_hz, Readback, TuningPlan};
use crate::services::discovery::carriers::BINS;

const LO: u64 = 858_100_000;
const CC: u64 = 860_962_500;
/// How far the fake spectrum misplaces the channel: the loop's residual must take it out.
const SPECTRUM_BIAS_HZ: f64 = 40.0;

#[test]
fn a_dmr_site_is_measured_by_its_equaliser_with_or_without_an_identity() {
    let con_plus = ControlStatus { running: true, last_message_age_ms: Some(100), carrier_offset_hz: Some(-456.0), ..ControlStatus::default() };
    assert_eq!(Source::of(&con_plus), Some(Source::DmrEqualiser));
    assert_eq!(Source::of(&ControlStatus { last_message_age_ms: Some(5_000), ..con_plus.clone() }), None, "nothing decoded lately");
    assert_eq!(Source::of(&ControlStatus { carrier_offset_hz: None, ..con_plus }), None, "no sync yet");
    assert_eq!(Source::of(&decoded_p25()), Some(Source::P25Loop));
}

#[test]
fn ppm_and_shift_are_inverses() {
    for ppm in [-0.6967, 0.0, 0.42] {
        let shift = lo_shift_hz(ppm, LO) as f64;
        assert!((ppm_of(shift, LO) - ppm).abs() < 2e-3, "{ppm}");
    }
}

#[test]
fn the_trimmed_mean_ignores_the_tails() {
    let mut s = vec![10.0; 18];
    s.push(1_000.0);
    s.push(-1_000.0);
    assert_eq!(trimmed_mean(&s, 0.1), Some(10.0));
    assert_eq!(trimmed_mean(&[], 0.1), None);
}

/// A DC-centred frame with a parabolic (in dB) peak centred on `offset_hz`.
fn frame_db(sample_rate: f64, offset_hz: f64) -> Vec<f32> {
    let bin_hz = sample_rate / BINS as f64;
    let centre = (BINS / 2) as f64 + offset_hz / bin_hz;
    (0..BINS).map(|i| (-90.0 + (30.0 - 3.0 * (i as f64 - centre).powi(2)).max(0.0)) as f32).collect()
}

#[test]
fn the_peak_is_found_between_bins() {
    let (offset, db) = peak_near(&frame_db(12e6, 2_863_100.0), 12e6, 2_862_500.0, SEARCH_HZ).unwrap();
    assert!((offset - 2_863_100.0).abs() < 1.0, "{offset}");
    assert!(db > -61.0);
    // Outside the search window it is not looked for.
    let far = peak_near(&frame_db(12e6, 2_900_000.0), 12e6, 2_862_500.0, SEARCH_HZ).unwrap();
    assert!((far.0 - 2_862_500.0).abs() > 5_000.0 || far.1 < -89.0);
}

#[test]
fn the_tracker_applies_only_near_its_calibration() {
    let anchor = ppm_of(500.0, LO);
    let near = ppm_of(530.0, LO);
    let far = ppm_of(600.0, LO);
    assert_eq!(decide(near, anchor, Some(anchor), 50, LO, true), Decision::Apply(near));
    assert_eq!(decide(far, anchor, Some(anchor), 50, LO, true), Decision::Hold("outside the anchor"));
    assert_eq!(decide(far, anchor, Some(anchor), 0, LO, true), Decision::Apply(far));
    assert_eq!(decide(near, anchor, None, 50, LO, true), Decision::Hold("not calibrated since start"));
    assert_eq!(decide(near, anchor, Some(anchor), 50, LO, false), Decision::Hold("tracking off"));
    assert_eq!(decide(anchor + 1e-4, anchor, Some(anchor), 50, LO, true), Decision::Hold("within 2 Hz"));
    // Never beyond the envelope.
    assert_eq!(decide(3.0, 0.9, None, 0, LO, true), Decision::Hold("not calibrated since start"));
    assert_eq!(decide(3.0, 0.5, Some(0.9), 0, LO, true), Decision::Apply(1.0));
}

#[test]
fn tracker_samples_restart_on_a_move_or_another_site() {
    let mut t = Tracker::default();
    let now = Instant::now();
    for _ in 0..MIN_SAMPLES {
        t.sample(now, (LO, CC), 0.5);
    }
    assert_eq!(t.estimate(), Some(0.5));
    t.sample(now, (LO + 1_000_000, CC), 0.5);
    assert_eq!(t.len(), 1, "another tuning starts over");
    t.moved(now);
    t.sample(now + Duration::from_secs(1), (LO + 1_000_000, CC), 0.5);
    assert_eq!(t.len(), 0, "the loop is still reconverging");
    t.sample(now + SETTLE, (LO + 1_000_000, CC), 0.5);
    assert_eq!(t.len(), 1);
}

/// A radio whose crystal needs `true_shift` Hz at `LO`: the control channel shows in the spectrum
/// (40 Hz off, as a bin interpolation can be) and in the decoder's carrier offset by how far the
/// commanded shift is from it.
#[derive(Default)]
struct Radio {
    true_shift: StdMutex<f64>,
    commanded: StdMutex<u64>,
}

impl Radio {
    /// How far the commanded LO is above where the crystal needs it.
    fn error_hz(&self) -> f64 {
        let shift = *self.commanded.lock().unwrap() as f64 - LO as f64;
        shift - *self.true_shift.lock().unwrap()
    }
}

struct Fake {
    radio: Arc<Radio>,
}

/// Spectrometer words: a 47-bit mantissa per bin (exponent 0) for `db`.
fn words(db: &[f32]) -> Vec<u8> {
    db.iter().flat_map(|&d| (10f64.powf((f64::from(d) + 148.0) / 10.0) as u64).to_le_bytes()).collect()
}

impl RadioHw for Fake {
    async fn set_lo(&self, hz: u64) -> Result<()> {
        *self.radio.commanded.lock().unwrap() = hz;
        Ok(())
    }
    async fn set_rate(&self, _: u32, _: u32) -> Result<()> {
        Ok(())
    }
    async fn set_gain(&self, _: GainMode, _: Option<f64>) -> Result<()> {
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
        Readback::default()
    }
    async fn spectrum(&self) -> Option<Vec<u8>> {
        // The commanded LO sits `error` above where the crystal needs it: the channel shows that
        // much lower.
        Some(words(&frame_db(12e6, (CC - LO) as f64 - self.radio.error_hz() + SPECTRUM_BIAS_HZ)))
    }
}

fn decoded_p25() -> ControlStatus {
    ControlStatus {
        running: true,
        identity: Some(SiteIdentity::P25(P25Identity::default())),
        modulation: Some("lsm"),
        last_message_age_ms: Some(100),
        ..ControlStatus::default()
    }
}

async fn crystal(true_shift: f64, start_ppm: f64, dir: &std::path::Path) -> Arc<Crystal<Fake>> {
    let radio = Arc::new(Radio::default());
    *radio.true_shift.lock().unwrap() = true_shift;
    let tuner = Arc::new(Tuner::new(Fake { radio: radio.clone() }, start_ppm));
    tuner.apply(TuningPlan { preset: find_preset("12M").unwrap(), lo_hz: LO, control_hz: CC }).await.unwrap();
    let paths = Paths::new(dir, dir);
    let config = Arc::new(tokio::sync::Mutex::new(Config::load(&paths).unwrap()));
    // The LSM's carrier offset, signal minus NCO: a commanded LO too high puts the channel low.
    let control = move || ControlStatus { carrier_offset_hz: Some(-radio.error_hz()), ..decoded_p25() };
    Crystal::new(Deps {
        tuner,
        control: Arc::new(control),
        lease: RadioLease::default(),
        config,
        paths,
        log: Arc::new(EventLog::default()),
    })
}

#[tokio::test(start_paused = true)]
async fn a_calibration_lands_on_the_crystal_and_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    // Unit A's crystal (+597 Hz at the Clay LO) from a start 1.4 kHz off, beyond the loop's reach.
    let c = crystal(597.0, 1.0, dir.path()).await;
    let cal = c.calibrate().await.unwrap();
    assert_eq!(cal.source, Source::P25Loop);
    assert!(cal.residual_samples >= 10, "{cal:?}");
    // The spectrum step overshoots by the bias (the LO commanded that much high, the channel
    // that much low: signal minus NCO reads -bias); the loop reads it back and the result is
    // exact.
    assert!((cal.residual_hz.unwrap() + SPECTRUM_BIAS_HZ).abs() < 1.0, "{cal:?}");
    let shift = c.deps.tuner.tuning().lo_shift_hz;
    assert!((shift - 597).abs() <= 1, "LO shift {shift} Hz");
    let status = c.status().await;
    assert_eq!(status.anchor_ppm, Some(cal.ppm));
    // Kept for the next start.
    let stored = Config::load(&Paths::new(dir.path(), dir.path())).unwrap().state.value.crystal.unwrap();
    assert!((stored.ppm - cal.ppm).abs() < 1e-9 && stored.method == "calibration");
}

#[tokio::test(start_paused = true)]
async fn the_tracker_follows_a_drift_within_the_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let c = crystal(597.0, 1.0, dir.path()).await;
    c.calibrate().await.unwrap();
    // The crystal warms by 30 Hz.
    *c.deps.tuner.hw().radio.true_shift.lock().unwrap() = 627.0;
    tokio::time::sleep(SETTLE).await;
    for _ in 0..(MIN_SAMPLES * 2) {
        c.sample(Some(Source::P25Loop)).await;
    }
    c.update().await;
    let shift = c.deps.tuner.tuning().lo_shift_hz;
    assert!((shift - 627).abs() <= 1, "LO shift {shift} Hz");
    // 300 Hz more is outside the 50 Hz anchor: held.
    *c.deps.tuner.hw().radio.true_shift.lock().unwrap() = 927.0;
    tokio::time::sleep(SETTLE).await;
    for _ in 0..(MIN_SAMPLES * 2) {
        c.sample(Some(Source::P25Loop)).await;
    }
    c.update().await;
    assert_eq!(c.deps.tuner.tuning().lo_shift_hz, shift);
    assert!(c.status().await.last_decision.unwrap().contains("outside the anchor"));
}
