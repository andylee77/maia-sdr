//! Change 071: the sweep task (target only: it drives the hardware).

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::*;
use crate::httpd::AppState;
use crate::protocol::p25::control_channel::ControlChannelDecoder;
use crate::services::event_log::LogCategory;

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Start a sweep; the radio lease is taken until it ends.
pub fn spawn_scan(state: Arc<AppState>, req: ScanRequest) -> Result<u64, String> {
    if !state.radio_lease.take_sweep() {
        return Err("the radio is busy (a sweep is running)".into());
    }
    let id = {
        let mut d = state.discovery.lock().unwrap();
        let id = d.id + 1;
        *d = DiscoveryState {
            id,
            state: "sweeping".into(),
            started_unix_ms: now_unix_ms(),
            bands: req.bands(),
            ..Default::default()
        };
        id
    };
    state.event_log.push(
        LogCategory::System,
        format!("system finder: sweep {id} started ({} bands)", req.bands().len()),
        serde_json::json!({ "id": id, "bands": req.bands() }),
    );
    tokio::spawn(async move {
        let result = run(&state, &req).await;
        {
            let mut d = state.discovery.lock().unwrap();
            d.state = "restoring".into();
            d.probing_hz = None;
        }
        restore(&state).await;
        state.radio_lease.release();
        let summary = {
            let mut d = state.discovery.lock().unwrap();
            d.finished_unix_ms = now_unix_ms();
            match result {
                Ok(()) if d.cancel => d.state = "cancelled".into(),
                Ok(()) => d.state = "done".into(),
                Err(e) => {
                    d.state = "error".into();
                    d.error = Some(e);
                }
            }
            format!(
                "system finder: sweep {} {}: {} P25 sites, {} other carriers, {} probed",
                d.id, d.state, d.sites.len(), d.other.len(), d.probed
            )
        };
        state.event_log.push(LogCategory::System, summary, serde_json::json!({ "id": id }));
    });
    Ok(id)
}

fn cancelled(state: &AppState) -> bool {
    state.discovery.lock().map(|d| d.cancel).unwrap_or(true)
}

fn update(state: &AppState, f: impl FnOnce(&mut DiscoveryState)) {
    if let Ok(mut d) = state.discovery.lock() {
        f(&mut d);
    }
}

/// The LO as the DDCs see it (crystal trim removed).
fn lo_eff(state: &AppState) -> f64 {
    (state.current_rx_lo.load(Ordering::Relaxed) - state.current_lo_shift_hz.load(Ordering::Relaxed)) as f64
}

async fn new_system(state: &AppState) {
    state.decoder.write().await.new_system();
    state.lsm_decoder.write().await.new_system();
}

/// Put the window at `lo` (16 MSPS) with the control channel at the LO.
async fn move_lo(state: &AppState, lo: u64) -> Result<(), String> {
    let shift = state.current_lo_shift_hz.load(Ordering::Relaxed);
    state.current_control_freq.store(lo, Ordering::Relaxed);
    let body = crate::httpd::api::tuning::PresetBody {
        preset: SWEEP_PRESET.into(),
        center_freq_hz: Some((lo as i64 + shift) as u64),
        gain_mode: None,
        gain_db: None,
    };
    let (status, reply) = crate::httpd::api::tuning::apply_preset(state, body).await;
    new_system(state).await;
    if !status.is_success() {
        return Err(format!("LO to {lo}: {}", reply.get("errors").map(|e| e.to_string()).unwrap_or_default()));
    }
    Ok(())
}

/// Point the control DDC at `freq` without moving the LO.
async fn tune_control(state: &AppState, freq: u64) -> Result<(), String> {
    let nco = freq as f64 - lo_eff(state);
    let sr = state.current_sample_rate_hz.load(Ordering::Relaxed) as f64;
    {
        let core = state.ip_core.lock().await;
        core.set_ddc_frequency(nco, sr).map_err(|e| e.to_string())?;
    }
    state.current_control_freq.store(freq, Ordering::Relaxed);
    new_system(state).await;
    Ok(())
}

/// `n` spectrometer frames (dB per bin) taken after the LO settled.
async fn grab_frames(state: &AppState, n: usize) -> Vec<Vec<f32>> {
    let mut out = Vec::new();
    let mut discard = 2;
    let deadline = Instant::now() + Duration::from_secs(4);
    while out.len() < n && Instant::now() < deadline {
        let buf = {
            let mut core = state.ip_core.lock().await;
            core.read_wideband_spec_buffer().map(|b| b.to_vec())
        };
        match buf {
            Some(b) if discard > 0 => {
                let _ = b;
                discard -= 1;
            }
            Some(b) => {
                let db = crate::services::spectrum::wideband_power_db(&b);
                if !db.is_empty() {
                    out.push(db);
                }
            }
            None => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    out
}

async fn run(state: &Arc<AppState>, req: &ScanRequest) -> Result<(), String> {
    let uh = crate::services::lo_plan::usable_half_hz(SWEEP_RATE_HZ) as f64;
    let bands = req.bands();
    let steps = plan_steps(&bands, uh);
    update(state, |d| d.steps = steps.len());
    // Frequencies already probed (within 3 kHz).
    let mut probed: Vec<u64> = Vec::new();
    let near = |list: &[u64], f: u64| list.iter().any(|&p| (p as i64 - f as i64).abs() <= 3_000);
    let mut budget = req.max_candidates;
    {
        let core = state.ip_core.lock().await;
        core.set_wideband_spec_enable(true);
    }
    for (k, &lo) in steps.iter().enumerate() {
        if cancelled(state) {
            return Ok(());
        }
        update(state, |d| {
            d.step = k + 1;
            d.state = "sweeping".into();
        });
        move_lo(state, lo).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let frames = grab_frames(state, req.frames).await;
        let mut carriers = find_carriers(&frames, lo_eff(state), SWEEP_RATE_HZ as f64, uh, 12.0, 0.8);
        carriers.sort_by(|a, b| b.level_db.partial_cmp(&a.level_db).unwrap_or(std::cmp::Ordering::Equal));
        carriers.retain(|c| !near(&probed, c.freq_hz) && bands.iter().any(|&(a, b)| c.freq_hz >= a && c.freq_hz <= b));
        carriers.truncate(budget);
        budget -= carriers.len();
        update(state, |d| {
            d.carriers += carriers.len();
            d.to_probe += carriers.len();
            d.state = "probing".into();
        });
        for c in carriers {
            if cancelled(state) {
                return Ok(());
            }
            probed.push(c.freq_hz);
            probe(state, req, c.freq_hz, c.level_db, false).await?;
        }
    }
    // Neighbours' control channels in the bands that no spectrum pass
    // turned up (weak, or on a DC spur).
    let neighbours: Vec<u64> = {
        let d = state.discovery.lock().unwrap();
        let mut v: Vec<u64> = d
            .sites
            .iter()
            .flat_map(|s| s.neighbours.iter().filter_map(|n| n.freq_hz))
            .filter(|f| bands.iter().any(|&(a, b)| *f >= a && *f <= b))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let neighbours: Vec<u64> = neighbours.into_iter().filter(|f| !near(&probed, *f)).collect();
    update(state, |d| d.to_probe += neighbours.len());
    for f in neighbours {
        if cancelled(state) {
            return Ok(());
        }
        if (f as f64 - lo_eff(state)).abs() > uh - 100_000.0 {
            // Keep it off the DC spur: LO 1 MHz away.
            move_lo(state, f + 1_000_000).await?;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        probed.push(f);
        probe(state, req, f, 0.0, true).await?;
    }
    Ok(())
}

/// Decode one candidate: P25 or not; a P25 site's identity.
async fn probe(state: &Arc<AppState>, req: &ScanRequest, freq: u64, level_db: f32, via_neighbour: bool) -> Result<(), String> {
    update(state, |d| d.probing_hz = Some(freq));
    tune_control(state, freq).await?;
    let count = |d: &ControlChannelDecoder| (d.tsbk_crc_ok, d.tsbk_crc_failures);
    let nids = |d: &ControlChannelDecoder| d.nid_decoded_ok;
    let c0 = count(&*state.decoder.read().await);
    let l0 = count(&*state.lsm_decoder.read().await);
    let (cn0, ln0) = (nids(&*state.decoder.read().await), nids(&*state.lsm_decoder.read().await));
    let t0 = Instant::now();
    tokio::time::sleep(Duration::from_millis(req.probe_ms)).await;
    let tsbks = state.decoder.read().await.tsbk_crc_ok - c0.0 + state.lsm_decoder.read().await.tsbk_crc_ok - l0.0;
    if tsbks < 3 {
        // NIDs on either decoder (each counted from its own start).
        let voice_nids = nids(&*state.decoder.read().await)
            .saturating_sub(cn0)
            .max(nids(&*state.lsm_decoder.read().await).saturating_sub(ln0));
        // Silent: a steady carrier of something else, or a call that
        // ended between the spectrum pass and the probe (not listed).
        let steady = voice_nids < 3 && !via_neighbour && still_there(state, freq).await;
        update(state, |d| {
            d.probed += 1;
            let c = Carrier { freq_hz: freq, level_db, persistence: 1.0 };
            if voice_nids >= 3 {
                d.p25_voice.push(c);
            } else if steady {
                d.other.push(c);
            }
        });
        return Ok(());
    }
    // P25: wait for the identity, then a little longer for the band
    // table and the neighbour list.
    let deadline = t0 + Duration::from_millis(req.identity_ms.max(req.probe_ms));
    let mut identified_at: Option<Instant> = None;
    loop {
        let known = {
            let c = state.decoder.read().await;
            let l = state.lsm_decoder.read().await;
            let ok = |d: &ControlChannelDecoder| {
                d.system.wacn.is_some() && d.system.system_id.is_some() && d.system.site_id.is_some() && d.system.nac.is_some()
            };
            ok(&c) || ok(&l)
        };
        if known && identified_at.is_none() {
            identified_at = Some(Instant::now());
        }
        let now = Instant::now();
        if now >= deadline || identified_at.is_some_and(|t| now.duration_since(t) >= Duration::from_secs(3)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let secs = t0.elapsed().as_secs_f64();
    let c1 = count(&*state.decoder.read().await);
    let l1 = count(&*state.lsm_decoder.read().await);
    let (c_ok, c_fail) = (c1.0 - c0.0, c1.1 - c0.1);
    let (l_ok, l_fail) = (l1.0 - l0.0, l1.1 - l0.1);
    // Which demodulator suits the site: by CRC pass rate (the C4FM path
    // starts a moment later after a retune, so raw counts favour LSM).
    let rate = |ok: u64, fail: u64| ok as f64 / (ok + fail).max(1) as f64;
    let c4fm_better = if c_ok + c_fail >= 10 && l_ok + l_fail >= 10 {
        rate(c_ok, c_fail) > rate(l_ok, l_fail)
    } else {
        c_ok > l_ok
    };
    let found = {
        let dec = if c4fm_better { state.decoder.read().await } else { state.lsm_decoder.read().await };
        let s = &dec.system;
        let (ok, fail) = if c4fm_better { (c_ok, c_fail) } else { (l_ok, l_fail) };
        let mut bands: Vec<crate::services::sites::IdenBand> = dec
            .bands
            .values()
            .map(|b| crate::services::sites::IdenBand {
                identifier: b.identifier,
                base_frequency_hz: b.base_frequency_hz,
                channel_spacing_hz: b.channel_spacing_hz,
                bandwidth_hz: b.bandwidth_hz,
                transmit_offset_hz: b.transmit_offset_hz as i64,
            })
            .collect();
        bands.sort_by_key(|b| b.identifier);
        let mut secondary: Vec<u64> = [s.control_channel, s.secondary_cch_a, s.secondary_cch_b]
            .iter()
            .flatten()
            .filter_map(|c| dec.channel_to_frequency(*c))
            .collect();
        secondary.sort_unstable();
        secondary.dedup();
        // The site's own announced control channel when the spectrum
        // estimate is that channel (exact, for the site file).
        let announced = s
            .control_channel
            .and_then(|c| dec.channel_to_frequency(c))
            .filter(|f| (*f as i64 - freq as i64).abs() <= 5_000);
        FoundSite {
            freq_hz: announced.unwrap_or(freq),
            level_db,
            modulation: if c4fm_better { "C4FM".into() } else { "LSM".into() },
            tsbk_per_s: ok as f64 / secs,
            crc_pct: 100.0 * ok as f64 / (ok + fail).max(1) as f64,
            nac: s.nac.map(|n| n.0),
            wacn: s.wacn,
            system_id: s.system_id,
            rfss_id: s.rfss_id,
            site_id: s.site_id,
            lra: s.lra,
            bands,
            neighbours: s
                .neighbours
                .iter()
                .map(|((sys, rfss, site), n)| FoundNeighbour {
                    system_id: format!("{sys:03X}"),
                    rfss_id: *rfss,
                    site_id: *site,
                    freq_hz: dec.channel_to_frequency(n.channel),
                })
                .collect(),
            secondary_hz: secondary,
            existing_site: None,
            via_neighbour,
        }
    };
    // A few TSBKs but no identity (e.g. a strong neighbour's control
    // channel leaking in): not a site of its own.
    if found.wacn.is_none() || found.system_id.is_none() || found.site_id.is_none() {
        update(state, |d| {
            d.probed += 1;
            d.p25_voice.push(Carrier { freq_hz: freq, level_db, persistence: 1.0 });
        });
        return Ok(());
    }
    let found = FoundSite { existing_site: match_site(&found), ..found };
    update(state, |d| {
        d.probed += 1;
        // The same site found twice (e.g. a secondary control channel):
        // keep the stronger.
        if let Some(prev) = d.sites.iter_mut().find(|x| x.key() == found.key() && found.wacn.is_some()) {
            if found.tsbk_per_s > prev.tsbk_per_s {
                *prev = found;
            }
        } else {
            d.sites.push(found);
        }
    });
    Ok(())
}

/// Is a carrier still at `freq` (12 dB over the floor in 2 of 3 new
/// spectrometer frames)?
async fn still_there(state: &AppState, freq: u64) -> bool {
    let frames = grab_frames(state, 3).await;
    let found = find_carriers(&frames, lo_eff(state), SWEEP_RATE_HZ as f64, f64::MAX, 12.0, 0.66);
    found.iter().any(|c| (c.freq_hz as i64 - freq as i64).abs() <= 8_000)
}

/// The site file that describes this site, if any.
fn match_site(f: &FoundSite) -> Option<String> {
    for name in crate::services::sites::list_sites() {
        let Ok(s) = crate::services::sites::load_site(&name) else { continue };
        let same_id = f.wacn.is_some()
            && s.wacn == f.wacn
            && s.system_id.map(|v| v as u16) == f.system_id
            && s.site_id == f.site_id
            && s.rfss_id == f.rfss_id;
        let same_cc = (s.control_freq_hz as i64 - f.freq_hz as i64).abs() <= 3_000;
        if same_id || same_cc {
            return Some(name);
        }
    }
    None
}

/// Back to the active site: its control channel and planned window.
async fn restore(state: &Arc<AppState>) {
    let site = state.active_site.read().await.clone();
    let cc = site.as_ref().map(|s| s.control_freq_hz).unwrap_or(state.boot_control_freq);
    state.current_control_freq.store(cc, Ordering::Relaxed);
    let body = crate::httpd::api::tuning::PresetBody {
        preset: if site.is_some() { "auto".into() } else { "8M".into() },
        center_freq_hz: None,
        gain_mode: None,
        gain_db: None,
    };
    let (status, reply) = crate::httpd::api::tuning::apply_preset(state, body).await;
    new_system(state).await;
    if !status.is_success() {
        tracing::warn!("system finder: restore failed: {reply}");
    }
}
