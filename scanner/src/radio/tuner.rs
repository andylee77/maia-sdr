//! The only code that moves the radio: the AD9361 LO, sample rate, bandwidth and gain, the
//! control DDC, and the traffic lanes' NCOs.
//!
//! Frequencies are planned on the nominal LO. The crystal error is corrected by commanding the
//! AD9361 `lo_shift_hz` away from it (`-ppm·LO`), which puts the real LO on the nominal one; every
//! NCO is then `channel − nominal LO`. The shift scales with the LO, so it is recomputed from the
//! ppm on every LO move.
//!
//! The tuner remembers what each lane's NCO holds. Any change under the lanes (a new preset, an
//! LO move) clears that, so the next retune always writes the NCO.

use std::future::Future;
use std::sync::Mutex;

use anyhow::{bail, Result};
use serde::Serialize;
use tokio::sync::watch;

use crate::hardware::ad9361::GainMode;
use crate::hardware::p25core::Lane;
use crate::hardware::presets::DdcPreset;

/// What the radio is asked to do when a site goes live or the window moves.
#[derive(Debug, Clone, Copy)]
pub struct TuningPlan {
    pub preset: &'static DdcPreset,
    /// Nominal LO (before the crystal shift).
    pub lo_hz: u64,
    pub control_hz: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Gain {
    pub mode: &'static str,
    /// Manual mode only.
    pub db: Option<f64>,
}

/// The radio as tuned, published on every change.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tuning {
    pub preset: Option<&'static str>,
    pub sample_rate_hz: u32,
    /// Nominal LO; the AD9361 is commanded `lo_hz + lo_shift_hz`.
    pub lo_hz: u64,
    pub lo_shift_hz: i64,
    pub crystal_ppm: f64,
    pub control_hz: u64,
    pub gain: Option<Gain>,
    /// The frequency each lane's NCO holds (`None`: unknown, the next retune writes it).
    pub lanes: [Option<u64>; 2],
    pub rev: u64,
}

impl Tuning {
    /// The NCO offset that brings `freq_hz` to baseband.
    pub fn nco_offset(&self, freq_hz: u64) -> f64 {
        freq_hz as f64 - self.lo_hz as f64
    }

    /// Is `freq_hz` inside the DDC's ±sample_rate/2 window?
    pub fn in_window(&self, freq_hz: u64) -> bool {
        self.nco_offset(freq_hz).abs() <= self.sample_rate_hz as f64 / 2.0
    }
}

/// What the hardware reports, to check the tuning against.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Readback {
    pub lo_hz: Option<u64>,
    pub gain_db: Option<f64>,
    pub rssi_db: Option<f64>,
    pub gain_mode: Option<&'static str>,
    pub control_nco_hz: Option<f64>,
    pub control_lsm: Option<crate::hardware::p25core::LsmControl>,
    /// NAC and DUID of the latest frame the control chain's gateware LSM decoded.
    pub control_nid: Option<(u16, u8)>,
    pub control_status: Option<crate::hardware::p25core::LsmStatus>,
    pub lane_nco_hz: [Option<f64>; 2],
    pub lane_lsm: [Option<crate::hardware::p25core::LsmControl>; 2],
}

/// The crystal correction at an LO: commanding `LO + shift` puts the real LO on `LO`.
pub fn lo_shift_hz(ppm: f64, lo_hz: u64) -> i64 {
    (-ppm * 1e-6 * lo_hz as f64).round() as i64
}

/// The hardware operations the tuner needs.
pub trait RadioHw: Send + Sync {
    fn set_lo(&self, hz: u64) -> impl Future<Output = Result<()>> + Send;
    fn set_rate(&self, sample_rate_hz: u32, rf_bandwidth_hz: u32) -> impl Future<Output = Result<()>> + Send;
    fn set_gain(&self, mode: GainMode, db: Option<f64>) -> impl Future<Output = Result<()>> + Send;
    /// Load `preset` into the control DDC with its NCO at `nco_hz`.
    fn configure_control(&self, preset: &'static DdcPreset, nco_hz: f64) -> impl Future<Output = Result<()>> + Send;
    fn set_control_nco(&self, nco_hz: f64, sample_rate_hz: u32) -> impl Future<Output = Result<()>> + Send;
    /// Load `preset` into every lane's DDC, NCO 0, input on.
    fn configure_lanes(&self, preset: &'static DdcPreset) -> impl Future<Output = Result<()>> + Send;
    fn retune_lane(&self, lane: Lane, nco_hz: f64, sample_rate_hz: u32, reset: bool) -> impl Future<Output = Result<()>> + Send;
    fn pause_lane(&self, lane: Lane) -> impl Future<Output = Result<()>> + Send;
    /// What the hardware holds now (`sample_rate_hz` converts NCO words).
    fn readback(&self, sample_rate_hz: u32) -> impl Future<Output = Readback> + Send;
    /// A lane's carrier loop, when the hardware can tell.
    fn lane_pll(&self, _lane: Lane) -> impl Future<Output = Option<LanePll>> + Send {
        async { None }
    }
    /// The control chain's carrier loop and AGC, when the hardware can tell.
    fn control_loop(&self) -> impl Future<Output = Option<ControlLoop>> + Send {
        async { None }
    }
    /// The wideband spectrometer's newest frame (its raw words), once; `None` until another
    /// completes.
    fn spectrum(&self) -> impl Future<Output = Option<Vec<u8>>> + Send {
        async { None }
    }
}

/// A lane's carrier loop: its phase correction per symbol and the gateware's clamp (Q2.13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanePll {
    pub pll_q213: i16,
    pub clamp_q213: i32,
}

impl LanePll {
    /// Half the clamp or more: the loop ran off on noise (a parked lane keeps demodulating after
    /// the carrier drops).
    pub fn hot(&self) -> bool {
        i32::from(self.pll_q213).abs() >= self.clamp_q213 / 2
    }
}

/// The control chain's LSM: its carrier loop's phase correction per symbol (Q2.13) and its AGC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlLoop {
    pub pll_q213: i16,
    /// Q9.7.
    pub agc_gain: u16,
    /// Q1.15.
    pub agc_mag: u16,
}

impl ControlLoop {
    /// Gain times input magnitude: about 1 to 3 while a burst is on the channel, 0.1 to 0.3
    /// between bursts.
    pub fn agc_product(&self) -> f64 {
        f64::from(self.agc_gain) / 128.0 * f64::from(self.agc_mag) / 32768.0
    }
}

pub struct Tuner<H> {
    hw: H,
    state: watch::Sender<Tuning>,
    /// Serialises hardware sequences (an LO move must not interleave with a lane retune).
    sequence: tokio::sync::Mutex<()>,
    crystal_ppm: Mutex<f64>,
}

impl<H: RadioHw> Tuner<H> {
    pub fn new(hw: H, crystal_ppm: f64) -> Tuner<H> {
        let tuning = Tuning {
            preset: None,
            sample_rate_hz: 0,
            lo_hz: 0,
            lo_shift_hz: 0,
            crystal_ppm,
            control_hz: 0,
            gain: None,
            lanes: [None, None],
            rev: 0,
        };
        Tuner {
            hw,
            state: watch::channel(tuning).0,
            sequence: tokio::sync::Mutex::new(()),
            crystal_ppm: Mutex::new(crystal_ppm),
        }
    }

    pub fn tuning(&self) -> Tuning {
        self.state.borrow().clone()
    }

    /// The hardware, for the stream readers (they read, never tune).
    pub fn hw(&self) -> &H {
        &self.hw
    }

    fn publish(&self, f: impl FnOnce(&mut Tuning)) -> Tuning {
        self.state.send_modify(|t| {
            f(t);
            t.rev += 1;
        });
        self.tuning()
    }

    /// Bring the radio to `plan`: LO (with the crystal shift), rate and bandwidth, the control
    /// DDC on the control channel, and the lanes reloaded and idle.
    pub async fn apply(&self, plan: TuningPlan) -> Result<Tuning> {
        let _seq = self.sequence.lock().await;
        let ppm = *self.crystal_ppm.lock().unwrap_or_else(|p| p.into_inner());
        let shift = lo_shift_hz(ppm, plan.lo_hz);
        let rate = plan.preset.sample_rate_hz;
        let nco = plan.control_hz as f64 - plan.lo_hz as f64;
        if nco.abs() > rate as f64 / 2.0 {
            bail!("control channel {} Hz is outside the {} window at LO {}", plan.control_hz, plan.preset.name, plan.lo_hz);
        }
        self.hw.set_lo((plan.lo_hz as i64 + shift) as u64).await?;
        self.hw.set_rate(rate, plan.preset.rf_bandwidth_hz).await?;
        self.hw.configure_control(plan.preset, nco).await?;
        // The lanes' NCOs are 0 from here on.
        let lanes_result = self.hw.configure_lanes(plan.preset).await;
        let tuning = self.publish(|t| {
            t.preset = Some(plan.preset.name);
            t.sample_rate_hz = rate;
            t.lo_hz = plan.lo_hz;
            t.lo_shift_hz = shift;
            t.crystal_ppm = ppm;
            t.control_hz = plan.control_hz;
            t.lanes = [None, None];
        });
        lanes_result?;
        Ok(tuning)
    }

    /// Move the control DDC within the current window (another control channel of the site).
    pub async fn set_control(&self, control_hz: u64) -> Result<Tuning> {
        let _seq = self.sequence.lock().await;
        let t = self.tuning();
        if t.preset.is_none() || !t.in_window(control_hz) {
            bail!("control channel {control_hz} Hz is outside the current window");
        }
        self.hw.set_control_nco(t.nco_offset(control_hz), t.sample_rate_hz).await?;
        Ok(self.publish(|t| t.control_hz = control_hz))
    }

    /// A new crystal estimate: the LO is commanded again with the new shift. The lanes keep
    /// their NCOs (they are relative to the nominal LO), but their chains see a frequency step.
    pub async fn set_crystal_ppm(&self, ppm: f64) -> Result<Tuning> {
        let _seq = self.sequence.lock().await;
        *self.crystal_ppm.lock().unwrap_or_else(|p| p.into_inner()) = ppm;
        let t = self.tuning();
        if t.preset.is_none() {
            return Ok(self.publish(|t| t.crystal_ppm = ppm));
        }
        let shift = lo_shift_hz(ppm, t.lo_hz);
        if shift != t.lo_shift_hz {
            self.hw.set_lo((t.lo_hz as i64 + shift) as u64).await?;
        }
        Ok(self.publish(|t| {
            t.crystal_ppm = ppm;
            t.lo_shift_hz = shift;
        }))
    }

    pub async fn set_gain(&self, mode: GainMode, db: Option<f64>) -> Result<Tuning> {
        let _seq = self.sequence.lock().await;
        self.hw.set_gain(mode, db).await?;
        let db = if mode == GainMode::Manual { db } else { None };
        Ok(self.publish(|t| t.gain = Some(Gain { mode: mode.as_str(), db })))
    }

    /// Put a lane on `freq_hz`. `reset` clears its AGC, PLL and timing; a lane already holding
    /// the frequency only gets its chain switched on.
    pub async fn retune_lane(&self, lane: Lane, freq_hz: u64, reset: bool) -> Result<Tuning> {
        let _seq = self.sequence.lock().await;
        let t = self.tuning();
        if t.preset.is_none() || !t.in_window(freq_hz) {
            bail!("{freq_hz} Hz is outside the current window");
        }
        let holds = t.lanes[lane.index()] == Some(freq_hz);
        self.hw.retune_lane(lane, t.nco_offset(freq_hz), t.sample_rate_hz, reset || !holds).await?;
        Ok(self.publish(|t| t.lanes[lane.index()] = Some(freq_hz)))
    }

    pub async fn lane_pll(&self, lane: Lane) -> Option<LanePll> {
        self.hw.lane_pll(lane).await
    }

    pub async fn control_loop(&self) -> Option<ControlLoop> {
        self.hw.control_loop().await
    }

    /// Stop a lane's chain; its NCO stays where it is.
    pub async fn pause_lane(&self, lane: Lane) -> Result<()> {
        let _seq = self.sequence.lock().await;
        self.hw.pause_lane(lane).await
    }

    pub async fn readback(&self) -> Readback {
        self.hw.readback(self.tuning().sample_rate_hz).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::presets::find_preset;

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn log(&self, s: String) -> Result<()> {
            self.calls.lock().unwrap().push(s);
            Ok(())
        }
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.calls.lock().unwrap())
        }
    }

    impl RadioHw for Fake {
        async fn set_lo(&self, hz: u64) -> Result<()> {
            self.log(format!("lo {hz}"))
        }
        async fn set_rate(&self, sr: u32, bw: u32) -> Result<()> {
            self.log(format!("rate {sr} bw {bw}"))
        }
        async fn set_gain(&self, mode: GainMode, db: Option<f64>) -> Result<()> {
            self.log(format!("gain {} {db:?}", mode.as_str()))
        }
        async fn configure_control(&self, p: &'static DdcPreset, nco: f64) -> Result<()> {
            self.log(format!("control {} {nco}", p.name))
        }
        async fn set_control_nco(&self, nco: f64, _sr: u32) -> Result<()> {
            self.log(format!("control nco {nco}"))
        }
        async fn configure_lanes(&self, p: &'static DdcPreset) -> Result<()> {
            self.log(format!("lanes {}", p.name))
        }
        async fn retune_lane(&self, lane: Lane, nco: f64, _sr: u32, reset: bool) -> Result<()> {
            self.log(format!("{lane} nco {nco} reset {reset}"))
        }
        async fn pause_lane(&self, lane: Lane) -> Result<()> {
            self.log(format!("{lane} pause"))
        }
        async fn readback(&self, _: u32) -> Readback {
            Readback::default()
        }
    }

    fn clay_plan() -> TuningPlan {
        TuningPlan { preset: find_preset("12M").unwrap(), lo_hz: 858_100_000, control_hz: 860_962_500 }
    }

    #[test]
    fn the_shift_corrects_a_crystal_error() {
        // Unit A: -0.697 ppm at 858.1 MHz commands the LO 598 Hz high.
        assert_eq!(lo_shift_hz(-0.6967293059852248, 858_100_000), 598);
        // The same crystal at the DMR site's LO.
        assert_eq!(lo_shift_hz(-0.6967293059852248, 452_728_440), 315);
    }

    #[tokio::test]
    async fn a_plan_tunes_in_order_with_the_shift() {
        let tuner = Tuner::new(Fake::default(), -0.6967293059852248);
        let t = tuner.apply(clay_plan()).await.unwrap();
        let bw = find_preset("12M").unwrap().rf_bandwidth_hz;
        assert_eq!(
            tuner.hw.take(),
            vec![
                "lo 858100598".to_string(),
                format!("rate 12000000 bw {bw}"),
                "control 12M 2862500".into(),
                "lanes 12M".into(),
            ]
        );
        assert_eq!((t.lo_hz, t.lo_shift_hz, t.preset), (858_100_000, 598, Some("12M")));
        assert_eq!(t.nco_offset(857_987_500), -112_500.0);
    }

    #[tokio::test]
    async fn a_lane_reloaded_under_it_is_always_retuned() {
        let tuner = Tuner::new(Fake::default(), 0.0);
        tuner.apply(clay_plan()).await.unwrap();
        tuner.hw.take();
        tuner.retune_lane(Lane::One, 857_987_500, false).await.unwrap();
        tuner.retune_lane(Lane::One, 857_987_500, false).await.unwrap();
        assert_eq!(tuner.hw.take(), vec!["lane 1 nco -112500 reset true", "lane 1 nco -112500 reset false"]);
        // A recentre reloads the lanes: the same channel is written and reset again.
        tuner.apply(TuningPlan { lo_hz: 858_200_000, ..clay_plan() }).await.unwrap();
        tuner.hw.take();
        tuner.retune_lane(Lane::One, 857_987_500, false).await.unwrap();
        assert_eq!(tuner.hw.take(), vec!["lane 1 nco -212500 reset true"]);
    }

    #[tokio::test]
    async fn out_of_window_requests_are_refused() {
        let tuner = Tuner::new(Fake::default(), 0.0);
        assert!(tuner.retune_lane(Lane::One, 857_987_500, true).await.is_err(), "not tuned yet");
        tuner.apply(clay_plan()).await.unwrap();
        assert!(tuner.retune_lane(Lane::Two, 851_000_000, true).await.is_err());
        assert!(tuner.apply(TuningPlan { control_hz: 870_000_000, ..clay_plan() }).await.is_err());
    }

    #[tokio::test]
    async fn a_new_crystal_estimate_moves_the_lo_only() {
        let tuner = Tuner::new(Fake::default(), 0.0);
        tuner.apply(clay_plan()).await.unwrap();
        tuner.hw.take();
        let t = tuner.set_crystal_ppm(-0.5).await.unwrap();
        assert_eq!(t.lo_shift_hz, 429);
        assert_eq!(tuner.hw.take(), vec!["lo 858100429"]);
        assert_eq!(tuner.tuning().nco_offset(860_962_500), 2_862_500.0);
    }
}
