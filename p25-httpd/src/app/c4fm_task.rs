//! Change 071b: software C4FM on the control channel, and the choice
//! between it and the HDL LSM chain.
//!
//! The C4FM demodulator (`protocol::p25::c4fm`, SDRTrunk's) runs on its
//! own thread from the control IQ hub and feeds the second control
//! decoder (`AppState::decoder`). Both decoders run all the time; only
//! the active one publishes grants and TSBK events. In auto mode the one
//! passing more TSBK CRCs over the last 5 s wins, with hysteresis, so a
//! C4FM site (FPL, SLERS: 42-60 % on the LSM chain, 95-99 % here) moves
//! to it and an LSM site stays put.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::protocol::p25::control_channel::ControlChannelDecoder;

/// Modulation codes (`/api/modulation`): the setting is 0 = auto,
/// 1 = C4FM, 2 = LSM; the active decoder is 1 or 2.
pub const AUTO: u8 = 0;
pub const C4FM: u8 = 1;
pub const LSM: u8 = 2;

/// Auto mode: the other decoder must pass this many TSBKs in the window
/// and beat the active one by `SWITCH_RATIO`. A 5 s window flapped ~60
/// times an hour on a weak LSM site (unit B indoors), where the two are
/// close; hence 20 s, and a minimum dwell after a switch.
const WINDOW_S: usize = 20;
const MIN_TSBKS: u64 = 30;
const SWITCH_RATIO: f64 = 1.25;
/// After a switch, stay at least this long. A new control channel
/// (site switch, retune) clears the window and the dwell.
const MIN_DWELL: std::time::Duration = std::time::Duration::from_secs(60);

/// Runtime figures of the software C4FM path.
#[derive(Default)]
pub struct C4fmRuntime {
    pub chunks: AtomicU64,
    pub lagged: AtomicU64,
    pub resets: AtomicU64,
    /// Share of one core, x100.
    pub cpu_centi_pct: AtomicU64,
    /// Equaliser frequency offset, milliradians per symbol.
    pub pll_mrad: std::sync::atomic::AtomicI64,
}

/// The per-second choice: returns the decoder that should be active.
pub fn choose(active: u8, window: &VecDeque<(u64, u64)>) -> u8 {
    let (c4fm, lsm): (u64, u64) = window.iter().fold((0, 0), |a, w| (a.0 + w.0, a.1 + w.1));
    let (mine, other, other_code) = if active == C4FM { (c4fm, lsm, LSM) } else { (lsm, c4fm, C4FM) };
    if other >= MIN_TSBKS && other as f64 > mine as f64 * SWITCH_RATIO {
        other_code
    } else {
        active
    }
}

/// Make `active` (C4FM / LSM) the publishing decoder.
pub async fn set_active(
    active: u8,
    active_modulation: &AtomicU8,
    c4fm: &RwLock<ControlChannelDecoder>,
    lsm: &RwLock<ControlChannelDecoder>,
) {
    c4fm.write().await.active = active == C4FM;
    lsm.write().await.active = active == LSM;
    active_modulation.store(active, Ordering::Relaxed);
}

/// The mode task: applies the setting, and in auto mode picks the
/// decoder once a second.
pub fn spawn_modulation_task(
    mode: Arc<AtomicU8>,
    active_modulation: Arc<AtomicU8>,
    c4fm: Arc<RwLock<ControlChannelDecoder>>,
    lsm: Arc<RwLock<ControlChannelDecoder>>,
    event_log: Arc<crate::services::event_log::EventLog>,
    control_freq: Arc<AtomicU64>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut last = (0u64, 0u64);
        let mut window: VecDeque<(u64, u64)> = VecDeque::new();
        let mut channel = control_freq.load(Ordering::Relaxed);
        let mut last_switch: Option<std::time::Instant> = None;
        loop {
            tick.tick().await;
            let now = (c4fm.read().await.tsbk_crc_ok, lsm.read().await.tsbk_crc_ok);
            // Another control channel: judge it afresh.
            let ch = control_freq.load(Ordering::Relaxed);
            if ch != channel {
                channel = ch;
                window.clear();
                last_switch = None;
            }
            window.push_back((now.0.saturating_sub(last.0), now.1.saturating_sub(last.1)));
            last = now;
            while window.len() > WINDOW_S {
                window.pop_front();
            }
            let active = active_modulation.load(Ordering::Relaxed);
            let dwelling = last_switch.is_some_and(|t| t.elapsed() < MIN_DWELL);
            let want = match mode.load(Ordering::Relaxed) {
                C4FM => C4FM,
                LSM => LSM,
                _ if dwelling => active,
                _ => choose(if active == C4FM { C4FM } else { LSM }, &window),
            };
            let flags_ok = c4fm.read().await.active == (want == C4FM) && lsm.read().await.active == (want == LSM);
            if want != active || !flags_ok {
                set_active(want, &active_modulation, &c4fm, &lsm).await;
                if want != active {
                    last_switch = Some(std::time::Instant::now());
                    let (c, l) = window.iter().fold((0, 0), |a, w| (a.0 + w.0, a.1 + w.1));
                    let label = if want == C4FM { "C4FM" } else { "LSM" };
                    tracing::info!("control channel modulation -> {label} (TSBKs in {WINDOW_S} s: C4FM {c}, LSM {l})");
                    event_log.push(
                        crate::services::event_log::LogCategory::System,
                        format!("control channel decoded as {label} (TSBKs in {WINDOW_S} s: C4FM {c}, LSM {l})"),
                        serde_json::json!({ "active": label, "tsbk_c4fm": c, "tsbk_lsm": l }),
                    );
                }
            }
        }
    });
}

/// The software C4FM thread: control IQ -> C4FM demodulator -> `decoder`.
#[cfg(target_os = "linux")]
pub fn spawn_c4fm_control(
    hub: Arc<crate::app::iq_hub::IqHub>,
    decoder: Arc<RwLock<ControlChannelDecoder>>,
    control_freq: Arc<AtomicU64>,
    rx_lo: Arc<std::sync::atomic::AtomicI64>,
    rt: Arc<C4fmRuntime>,
) {
    use crate::protocol::p25::c4fm::C4fmDecoder;
    use tokio::sync::broadcast::error::RecvError;
    let spawned = std::thread::Builder::new().name("c4fm-cc".into()).spawn(move || {
        let mut rx = hub.subscribe();
        let mut c4fm = C4fmDecoder::new();
        let mut tuned = (control_freq.load(Ordering::Relaxed), rx_lo.load(Ordering::Relaxed));
        let mut busy = std::time::Duration::ZERO;
        let mut since = std::time::Instant::now();
        loop {
            let chunk = match rx.blocking_recv() {
                Ok(c) => c,
                Err(RecvError::Lagged(_)) => {
                    rt.lagged.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            // A retune: the equaliser's offset belongs to the old channel.
            let now_tuned = (control_freq.load(Ordering::Relaxed), rx_lo.load(Ordering::Relaxed));
            if now_tuned != tuned {
                tuned = now_tuned;
                c4fm.reset();
                rt.resets.fetch_add(1, Ordering::Relaxed);
            }
            let t0 = std::time::Instant::now();
            {
                let mut d = decoder.blocking_write();
                c4fm.process_iq_i16(&chunk, &mut *d);
            }
            busy += t0.elapsed();
            rt.chunks.fetch_add(1, Ordering::Relaxed);
            if since.elapsed() >= std::time::Duration::from_secs(5) {
                let pct = busy.as_secs_f64() / since.elapsed().as_secs_f64() * 100.0;
                rt.cpu_centi_pct.store((pct * 100.0) as u64, Ordering::Relaxed);
                rt.pll_mrad.store((c4fm.demod.pll() * 1000.0) as i64, Ordering::Relaxed);
                busy = std::time::Duration::ZERO;
                since = std::time::Instant::now();
            }
        }
        tracing::warn!("c4fm control thread exiting (IQ hub closed)");
    });
    if let Err(e) = spawned {
        tracing::error!("c4fm control thread not started: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_choice_has_hysteresis() {
        let w = |v: &[(u64, u64)]| v.iter().copied().collect::<VecDeque<_>>();
        // FPL-like: C4FM 99 %, LSM 42 % -> C4FM.
        assert_eq!(choose(LSM, &w(&[(40, 17), (40, 17), (40, 16)])), C4FM);
        // Clay-like: both ~equal -> stay.
        assert_eq!(choose(LSM, &w(&[(40, 39), (41, 40)])), LSM);
        assert_eq!(choose(C4FM, &w(&[(40, 39), (41, 40)])), C4FM);
        // Too few TSBKs to judge -> stay.
        assert_eq!(choose(LSM, &w(&[(5, 0)])), LSM);
        // LSM clearly better while on C4FM -> back.
        assert_eq!(choose(C4FM, &w(&[(10, 40), (12, 40)])), LSM);
    }
}
