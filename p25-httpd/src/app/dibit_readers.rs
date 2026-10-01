//! Dibit reader tasks — DMA readers that feed the P25 framers.
//!
//! Linux-only: both readers depend on `fpga::IpCore` and the DMA
//! ring accessors. Gated with `#![cfg(target_os = "linux")]` so the
//! Windows `cargo check` path still works.
//!
//! ## Phase 10.8 trim (2026-04-23)
//!
//! The PS C4FM control + traffic readers were retired along with the
//! HDL C4FM chain. Only the HDL LSM readers remain.
//!
//! - `spawn_hdl_lsm_control_reader` — HDL LSM reader that feeds the
//!   LSM control-channel decoder.
//! - `spawn_hdl_lsm_traffic_reader` — HDL LSM reader on the traffic
//!   chain with an `ImbeForwarder`-gated framer enable.
//!
//! ## Change 054 (2026-09-26): low-latency delivery + air-time gating
//!
//! Both readers now run one of three modes (runtime switch via
//! `--dibit-delivery` / `POST /api/dibit_delivery`, see
//! `app::dibit_airtime::DeliveryMode`):
//!
//! - `legacy` — the pre-054 path: whole 4 KiB sub-buffers (16384
//!   dibits = 3.41 s) on the DMA IRQ; the traffic TG gate is sampled
//!   once per batch. Kept for bench A/B comparisons.
//! - `poll` — position polling every `poll_ms` (default 40 ms, the IRQ
//!   is an extra wake hint): the reader tracks an absolute byte position
//!   and copies `[pos, safe_end)` where `safe_end` comes from the
//!   PREVIOUS poll's next-address reading minus 256 B (see
//!   `hardware::dibit_ring`). Dibits arrive ≈ burst fill (0–107 ms) +
//!   1–2 poll intervals after production instead of 0–3.41 s. Traffic
//!   gating stays live (sampled per chunk), like legacy.
//! - `airtime` (default) — `poll` delivery plus, on the traffic ring,
//!   chunk splitting at the chain epoch cuts recorded by the follower /
//!   lifecycle / IpCore hooks, so every dibit is decoded under the
//!   context in effect when it was on the air (`app::dibit_airtime`).
//!   On the control ring `airtime` behaves like `poll`.
//!
//! Every mode records per-ring delivery age (poll time − estimated
//! production time), resyncs and epoch statistics for
//! `/api/dibit_delivery`.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, RwLock};

use crate::app::dibit_airtime::{
    plan_chunk, unix_ms_now, unix_ms_of, unix_offset_us, AirtimeState, DeliveryMode,
    DibitDelivery, DibitRingShared, Step,
};
use crate::app::forensics::ForensicsRing;
use crate::app::imbe_forwarder::ImbeForwarder;
use crate::hardware::dibit_ring::{
    mono_us, ClockView, Resync, RingGeometry, RingTracker, BURST_BYTES, DIBITS_PER_BYTE,
};
use crate::hardware::fpga::{self, DibitRing};
use crate::protocol::p25::control_channel::ControlChannelDecoder;
use crate::services::event_log::{EventLog, LogCategory};

/// Legacy-mode IRQ wait timeout. Only bounds how quickly a mode switch
/// (or a lost `notify_waiters` wake-up) is noticed; a timeout wake with
/// no new sub-buffer delivers nothing.
const LEGACY_WAIT_TIMEOUT_MS: u64 = 500;

/// Periodic EventLog delivery summary cadence.
const SUMMARY_PERIOD_SECS: u64 = 300;

/// Zero-copy reinterpretation of a byte buffer as packed u64 dibit
/// words. Caller guarantees the buffer length is a multiple of 8.
fn bytemuck_cast(buffer: &[u8]) -> &[u64] {
    unsafe {
        std::slice::from_raw_parts(
            buffer.as_ptr() as *const u64,
            buffer.len() / 8,
        )
    }
}

fn word_at(bytes: &[u8], w: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&bytes[w * 8..w * 8 + 8]);
    u64::from_le_bytes(b)
}

/// Feed dibits `[start, end)` (absolute indices) of a chunk whose first
/// dibit has index `base` to the framer, whole words where aligned.
/// `on_word` gets the absolute index of the last dibit of each word run
/// before it is fed (air-time stamping).
fn feed_range(
    dec: &mut ControlChannelDecoder,
    bytes: &[u8],
    base: u64,
    start: u64,
    end: u64,
    mut on_word: impl FnMut(u64),
) {
    let mut i = start;
    while i < end {
        let rel = i - base;
        let w = (rel / 32) as usize;
        let k0 = rel % 32;
        let word_end = (i - k0 + 32).min(end);
        let word = word_at(bytes, w);
        on_word(word_end - 1);
        let n = word_end - i;
        if k0 == 0 && n == 32 {
            dec.process_dma_word(word);
        } else {
            for k in k0..k0 + n {
                dec.process_dibit(((word >> (2 * k)) & 0x03) as u8);
            }
        }
        i = word_end;
    }
}

/// Per-ring reader machinery shared by both tasks.
struct RingReader {
    ring: DibitRing,
    shared: Arc<DibitRingShared>,
    geom: RingGeometry,
    tracker: RingTracker,
    /// Mode currently running (`None` before the first activation).
    active: Option<DeliveryMode>,
    buf: Vec<u8>,
    last_summary: std::time::Instant,
    event_log: Arc<EventLog>,
}

/// What one iteration produced.
struct Delivery {
    /// Absolute dibit index of `bytes[0]`.
    start_idx: u64,
    bytes: Vec<u8>,
    resync: Option<Resync>,
    /// Epoch cuts claimed for this chunk (airtime traffic only).
    cuts: Vec<crate::app::dibit_airtime::EpochCut>,
    /// Legacy path: sub-buffer batches `(start_idx, bytes)`.
    legacy: Vec<(u64, Vec<u8>)>,
    /// The copy failed: the position advanced past data that was not
    /// decoded (caller resets the framer).
    copy_failed: bool,
}

impl RingReader {
    async fn new(
        ring: DibitRing,
        shared: Arc<DibitRingShared>,
        core: &Arc<Mutex<fpga::IpCore>>,
        event_log: Arc<EventLog>,
    ) -> Self {
        let geom = core.lock().await.dibit_ring_geometry(ring);
        if !geom.is_valid() {
            // Cannot track positions on an unexpected geometry: pin the
            // ring to the legacy path.
            tracing::error!(
                "dibit ring {:?} has unexpected geometry {:?}; forcing legacy delivery",
                ring, geom,
            );
            event_log.push(
                LogCategory::System,
                format!("dibit ring {} geometry invalid — legacy delivery forced", shared.label),
                serde_json::json!({
                    "ring": shared.label,
                    "sub_buffer_bytes": geom.sub_buffer_bytes,
                    "num_sub_buffers": geom.num_sub_buffers,
                }),
            );
            shared.set_requested_mode(DeliveryMode::Legacy);
        }
        RingReader {
            ring,
            shared,
            geom,
            tracker: RingTracker::new(geom),
            active: None,
            buf: Vec::with_capacity(4096),
            last_summary: std::time::Instant::now(),
            event_log,
        }
    }

    fn geometry_ok(&self) -> bool {
        self.geom.is_valid()
    }

    /// Switch modes if requested. Returns `Some(new_mode)` when a switch
    /// happened (caller resets its framer / epoch state).
    async fn maybe_switch(&mut self, core: &Arc<Mutex<fpga::IpCore>>) -> Option<DeliveryMode> {
        let mut requested = self.shared.requested_mode();
        if !self.geometry_ok() {
            requested = DeliveryMode::Legacy;
        }
        if self.active == Some(requested) {
            return None;
        }
        let old = self.active;
        {
            let mut c = core.lock().await;
            match (old, requested) {
                (Some(DeliveryMode::Legacy), DeliveryMode::Poll | DeliveryMode::Airtime) => {
                    // Continue exactly where the legacy path stopped: the
                    // start of the first sub-buffer it has not delivered.
                    if !self.tracker.started() {
                        let snap = c.dibit_ring_snapshot(self.ring);
                        let _ = self.tracker.poll(&snap, false);
                    }
                    if let Some(cur) = c.legacy_dibit_cursor(self.ring) {
                        let n = self.geom.num_sub_buffers;
                        let next = (cur as u64 % n + 1) % n;
                        self.tracker.rewind_to_offset(next * self.geom.sub_buffer_bytes);
                    }
                }
                (Some(DeliveryMode::Poll | DeliveryMode::Airtime), DeliveryMode::Legacy) => {
                    // Mark the sub-buffer holding the last delivered byte
                    // as delivered: no duplicates, the rest of that
                    // sub-buffer is skipped (framer reset by the caller).
                    if self.tracker.started() {
                        let ring_bytes = self.geom.ring_bytes();
                        let last = (self.tracker.pos() + ring_bytes - 1) % ring_bytes;
                        let idx = self.geom.sub_buffer_of(last) as u32;
                        c.set_legacy_dibit_cursor(self.ring, Some(idx));
                    } else {
                        c.set_legacy_dibit_cursor(self.ring, None);
                    }
                }
                _ => {}
            }
        }
        self.shared
            .activate_mode(requested, self.tracker.pos() * DIBITS_PER_BYTE);
        self.active = Some(requested);
        self.event_log.push(
            LogCategory::System,
            format!(
                "dibit delivery {}: {} -> {}",
                self.shared.label,
                old.map(|m| m.as_str()).unwrap_or("start"),
                requested.as_str(),
            ),
            serde_json::json!({
                "ring": self.shared.label,
                "from": old.map(|m| m.as_str()),
                "to": requested.as_str(),
                "pos_bytes": self.tracker.pos(),
            }),
        );
        Some(requested)
    }

    /// Wait for the next iteration. Returns true for an IRQ wake.
    async fn wait(&self, waiter: &fpga::InterruptWaiter, poll_ms: u32) -> bool {
        match self.active {
            Some(DeliveryMode::Legacy) | None => {
                tokio::time::timeout(
                    Duration::from_millis(LEGACY_WAIT_TIMEOUT_MS),
                    waiter.wait(),
                )
                .await
                .is_ok()
            }
            _ => {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(poll_ms as u64)) => false,
                    _ = waiter.wait() => true,
                }
            }
        }
    }

    /// One iteration under the IpCore lock: register snapshot, position
    /// update, copy, clock observation, epoch claim.
    async fn iterate(
        &mut self,
        core: &Arc<Mutex<fpga::IpCore>>,
        irq: bool,
        claim_cuts: bool,
    ) -> Delivery {
        let mut out = Delivery {
            start_idx: 0,
            bytes: Vec::new(),
            resync: None,
            cuts: Vec::new(),
            legacy: Vec::new(),
            copy_failed: false,
        };
        let legacy = self.active == Some(DeliveryMode::Legacy);
        let mut c = core.lock().await;
        let snap = c.dibit_ring_snapshot(self.ring);
        let res = self.tracker.poll(&snap, !legacy);
        if let Some(abs) = res.abs_next {
            self.shared.observe(snap.t_us, abs, snap.chain_enabled);
        }
        self.shared.note_poll(irq, res.phase_ok);
        out.resync = res.resync;

        if legacy {
            let (first, bufs) = c.read_dibit_buffers_indexed(self.ring);
            drop(c);
            if let (Some(first), Some(safe)) = (first, self.tracker.last_safe_end()) {
                let n = self.geom.num_sub_buffers;
                let sb = self.geom.sub_buffer_bytes;
                let count = bufs.len() as u64;
                let last_idx = (first as u64 + count - 1) % n;
                let end_off = ((last_idx + 1) % n) * sb;
                let end = self.geom.abs_at_or_before(safe + 3 * BURST_BYTES, end_off);
                let mut start = end.saturating_sub(count * sb);
                for b in bufs {
                    out.legacy.push((start * DIBITS_PER_BYTE, b));
                    start += sb;
                }
            } else {
                for b in bufs {
                    out.legacy.push((0, b));
                }
            }
            return out;
        }

        if let Some((s, e)) = res.deliver {
            out.start_idx = s * DIBITS_PER_BYTE;
            if e > s {
                self.buf.clear();
                match c.copy_dibit_ring(self.ring, s, e, &mut self.buf) {
                    Ok(()) => out.bytes = std::mem::take(&mut self.buf),
                    Err(err) => {
                        out.copy_failed = true;
                        self.shared.note_copy_error();
                        tracing::warn!("dibit ring {:?} copy failed: {err}", self.ring);
                    }
                }
            }
            if claim_cuts {
                out.cuts = self.shared.claim(e * DIBITS_PER_BYTE);
            }
        }
        drop(c);
        out
    }

    /// Publish a resync (EventLog + counters).
    fn report_resync(&self, r: &Resync) {
        let info = serde_json::json!({
            "ring": self.shared.label,
            "reason": r.reason.as_str(),
            "from_pos_bytes": r.from_pos,
            "to_pos_bytes": r.to_pos,
            "skipped_bytes": r.to_pos.saturating_sub(r.from_pos),
            "delta_bytes": r.delta_bytes,
            "elapsed_s": (r.elapsed_secs * 1000.0).round() / 1000.0,
            "unix_ms": unix_ms_now(),
        });
        self.event_log.push(
            LogCategory::System,
            format!(
                "dibit ring {} resync ({}): skipped {} B after {:.3} s",
                self.shared.label,
                r.reason.as_str(),
                r.to_pos.saturating_sub(r.from_pos),
                r.elapsed_secs,
            ),
            info.clone(),
        );
        self.shared
            .note_resync(info, r.to_pos.saturating_sub(r.from_pos));
    }

    /// Periodic delivery summary into the EventLog.
    fn maybe_summary(&mut self) {
        if self.last_summary.elapsed() < Duration::from_secs(SUMMARY_PERIOD_SECS) {
            return;
        }
        self.last_summary = std::time::Instant::now();
        let c = self.shared.counters();
        if c.dibits_delivered == 0 {
            return;
        }
        let age = self.shared.age_summary();
        self.event_log.push(
            LogCategory::System,
            format!(
                "dibit delivery {} ({}): age p50={} p99={} max={} ms, splits={} discarded={} resyncs={}",
                self.shared.label,
                self.active.map(|m| m.as_str()).unwrap_or("?"),
                age["p50_ms"], age["p99_ms"], age["max_ms"],
                c.epoch_splits, c.dibits_discarded_presettle, c.resyncs,
            ),
            serde_json::json!({
                "ring": self.shared.label,
                "mode": self.active.map(|m| m.as_str()),
                "age": age,
                "counters": c,
            }),
        );
    }
}

fn dibit_hist(words: &[u64], hist: &mut [u64; 4]) {
    for &word in words {
        for i in 0..32 {
            hist[((word >> (i * 2)) & 0x03) as usize] += 1;
        }
    }
}

pub fn spawn_hdl_lsm_control_reader(
    lsm_dibit_waiter: fpga::InterruptWaiter,
    lsm_dibit_core: Arc<Mutex<fpga::IpCore>>,
    lsm_dibit_decoder: Arc<RwLock<ControlChannelDecoder>>,
    delivery: Arc<DibitDelivery>,
    event_log: Arc<EventLog>,
) {
    tokio::spawn(async move {
        tracing::info!(
            "HDL LSM dibit reader + TSBK decoder task started (054: {} mode)",
            delivery.control.requested_mode().as_str(),
        );
        let mut rd = RingReader::new(
            DibitRing::Control,
            delivery.control.clone(),
            &lsm_dibit_core,
            event_log,
        )
        .await;
        let mut wakeups: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut hist = [0u64; 4];
        let mut pending_reset = false;
        loop {
            if rd.maybe_switch(&lsm_dibit_core).await.is_some() {
                pending_reset = true;
            }
            let irq = rd.wait(&lsm_dibit_waiter, delivery.poll_ms()).await;
            wakeups += 1;
            let d = rd.iterate(&lsm_dibit_core, irq, false).await;
            let now_us = mono_us();
            if let Some(r) = d.resync.as_ref() {
                rd.report_resync(r);
                pending_reset = true;
            }
            if d.copy_failed {
                pending_reset = true;
            }

            // Gather the words delivered this iteration, in order.
            let mut chunks: Vec<(u64, Vec<u8>)> = d.legacy;
            let legacy = rd.active == Some(DeliveryMode::Legacy);
            if !d.bytes.is_empty() {
                chunks.push((d.start_idx, d.bytes));
            }
            if chunks.is_empty() {
                rd.maybe_summary();
                continue;
            }
            let mut dec = lsm_dibit_decoder.write().await;
            if pending_reset {
                dec.reset_framer_state();
                rd.shared.note_framer_reset();
                pending_reset = false;
            }
            for (start_idx, bytes) in &chunks {
                let words: &[u64] = bytemuck_cast(bytes);
                dibit_hist(words, &mut hist);
                for &word in words {
                    dec.process_dma_word(word);
                }
                total_bytes += bytes.len() as u64;
                let n = bytes.len() as u64 * DIBITS_PER_BYTE;
                if !legacy || *start_idx != 0 {
                    rd.shared.record_delivery(now_us, *start_idx, start_idx + n, legacy);
                }
                rd.shared.note_fed(n, 0);
            }
            let (sync_hits, near, best, msgs) = (
                dec.sync_hits(),
                dec.sync_near_misses(),
                dec.best_sync_distance(),
                dec.recent_messages.len(),
            );
            drop(dec);

            let log_every = if legacy { 16 } else { 256 };
            if wakeups <= 5 || wakeups % log_every == 0 {
                let total_dibits: u64 = hist.iter().sum();
                let pct = |v: u64| -> f64 {
                    if total_dibits == 0 { 0.0 } else { 100.0 * v as f64 / total_dibits as f64 }
                };
                tracing::info!(
                    target: "p25_hdl_lsm",
                    "wake #{wakeups}: chunks={} (cum bytes={total_bytes}) \
                     hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}% \
                     | LSM decoder: sync_hits={sync_hits} near={near} \
                     best_dist={} recent_msgs={msgs}",
                    chunks.len(),
                    pct(hist[0]), pct(hist[1]), pct(hist[2]), pct(hist[3]),
                    if best == u32::MAX { 99 } else { best },
                );
            }
            rd.maybe_summary();
        }
    });
}

/// 2026-05-03 dual-DDC: traffic-side LSM dibit reader hanging off
/// `m_axi_traffic_lsm_dibit`. Mirror of the control-chain reader:
/// dedicated `traffic_ddc` → LsmDecimator2 → LPF → RRC → LsmDemod →
/// DibitPacker → DMA. PS-side parsing identical to control side; gate
/// framer dispatch on the call context so noise dibits don't drive
/// false NID events between calls (legacy / poll: live
/// `current_talkgroup != 0` per chunk; airtime: per air-time epoch).
#[allow(clippy::too_many_arguments)]
pub fn spawn_hdl_lsm_traffic_reader(
    traffic_lsm_dibit_waiter: fpga::InterruptWaiter,
    traffic_lsm_core: Arc<Mutex<fpga::IpCore>>,
    traffic_lsm_decoder_task: Arc<RwLock<ControlChannelDecoder>>,
    traffic_reader_imbe: Arc<ImbeForwarder>,
    // Change 066: chain 1 only (the forensics capture is single-chain).
    forensics: Option<Arc<ForensicsRing>>,
    delivery: Arc<DibitDelivery>,
    // Change 066: this chain's ring state (`DibitDelivery::traffic` /
    // `traffic2`); the ring is the forwarder's lane's.
    shared: Arc<DibitRingShared>,
    event_log: Arc<EventLog>,
    // Change 074: a data-only decoder fed what the call gate holds back
    // (the chain between calls): packet data (PDUs) flows then.
    data_decoder: Option<Arc<RwLock<ControlChannelDecoder>>>,
) {
    let lane = traffic_reader_imbe.lane;
    tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        tracing::info!(
            "{lane} LSM dibit reader + voice frame decoder task started \
             (054: {} mode)",
            shared.requested_mode().as_str(),
        );
        let mut rd = RingReader::new(
            DibitRing::of_lane(lane),
            shared,
            &traffic_lsm_core,
            event_log,
        )
        .await;
        let mut state = AirtimeState::new(traffic_reader_imbe.live_context());
        let mut wakeups: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut hist = [0u64; 4];
        let mut pending_reset = false;
        // Change 074: end (absolute dibit index) of the last range fed to
        // the data decoder; a gap resets its framer.
        let mut data_end: u64 = 0;
        loop {
            if let Some(m) = rd.maybe_switch(&traffic_lsm_core).await {
                pending_reset = true;
                if m == DeliveryMode::Airtime {
                    state = AirtimeState::new(traffic_reader_imbe.live_context());
                }
            }
            let mode = rd.active.unwrap_or(DeliveryMode::Legacy);
            let irq = rd.wait(&traffic_lsm_dibit_waiter, delivery.poll_ms()).await;
            wakeups += 1;
            let d = rd
                .iterate(&traffic_lsm_core, irq, mode == DeliveryMode::Airtime)
                .await;
            let now_us = mono_us();
            if let Some(r) = d.resync.as_ref() {
                rd.report_resync(r);
                pending_reset = true;
            }
            if d.copy_failed {
                pending_reset = true;
            }

            match mode {
                DeliveryMode::Legacy | DeliveryMode::Poll => {
                    let mut chunks: Vec<(u64, Vec<u8>)> = d.legacy;
                    if !d.bytes.is_empty() {
                        chunks.push((d.start_idx, d.bytes));
                    }
                    if chunks.is_empty() {
                        rd.maybe_summary();
                        continue;
                    }
                    // Idle gate: chain emits dibits even when no TG
                    // locked (RRC + LsmDemod can't tell live signal from
                    // traffic_ddc residual when no carrier is tuned).
                    // Sampled once per chunk (pre-054 semantics).
                    let locked = traffic_reader_imbe
                        .current_talkgroup
                        .load(Ordering::Relaxed) != 0;
                    let legacy = mode == DeliveryMode::Legacy;
                    let mut dec = traffic_lsm_decoder_task.write().await;
                    let mut ddec = match &data_decoder {
                        Some(d) => Some(d.write().await),
                        None => None,
                    };
                    if pending_reset {
                        dec.reset_framer_state();
                        rd.shared.note_framer_reset();
                        if let Some(dd) = ddec.as_mut() {
                            dd.reset_framer_state();
                        }
                        pending_reset = false;
                    }
                    for (start_idx, bytes) in &chunks {
                        let words: &[u64] = bytemuck_cast(bytes);
                        dibit_hist(words, &mut hist);
                        if let Some(f) = forensics.as_ref() {
                            f.record_dma_words(words);
                        }
                        let n = bytes.len() as u64 * DIBITS_PER_BYTE;
                        if locked {
                            for &word in words {
                                dec.process_dma_word(word);
                            }
                            rd.shared.note_fed(n, 0);
                        } else {
                            rd.shared.note_fed(0, n);
                            // Change 074: between calls, to the data decoder.
                            if let Some(dd) = ddec.as_mut() {
                                if *start_idx != data_end {
                                    dd.reset_framer_state();
                                }
                                for &word in words {
                                    dd.process_dma_word(word);
                                }
                                data_end = start_idx + n;
                            }
                        }
                        total_bytes += bytes.len() as u64;
                        if !legacy || *start_idx != 0 {
                            rd.shared
                                .record_delivery(now_us, *start_idx, start_idx + n, legacy);
                        }
                    }
                    drop(dec);
                    drop(ddec);
                }
                DeliveryMode::Airtime => {
                    if d.bytes.is_empty() && d.cuts.is_empty() {
                        // Nothing delivered and nothing claimed. (Cuts
                        // arrive with an empty chunk only after start-
                        // up, a mode switch or a copy error; they are
                        // applied at the chunk start below.)
                        rd.maybe_summary();
                        continue;
                    }
                    let bytes = d.bytes;
                    let i0 = d.start_idx;
                    let i1 = i0 + bytes.len() as u64 * DIBITS_PER_BYTE;
                    let words: &[u64] = bytemuck_cast(&bytes);
                    dibit_hist(words, &mut hist);
                    if let Some(f) = forensics.as_ref() {
                        f.record_dma_words(words);
                    }
                    rd.shared.record_delivery(now_us, i0, i1, false);
                    total_bytes += bytes.len() as u64;

                    let mut steps = plan_chunk(&mut state, i0, i1, &d.cuts);
                    if pending_reset {
                        steps.insert(0, Step::ResetFramer { at: i0 });
                        pending_reset = false;
                    }
                    let view: ClockView = rd.shared.clock_view();
                    let offset = unix_offset_us();
                    let mut latched: Option<u64> = None;
                    {
                        let mut dec = traffic_lsm_decoder_task.write().await;
                        let mut ddec = match &data_decoder {
                            Some(d) => Some(d.write().await),
                            None => None,
                        };
                        for step in &steps {
                            match step {
                                Step::ResetFramer { .. } => {
                                    dec.reset_framer_state();
                                    if let Some(dd) = ddec.as_mut() {
                                        dd.reset_framer_state();
                                    }
                                }
                                // Change 074: what the call gate holds back
                                // goes to the data decoder.
                                Step::Gate { start, end } => {
                                    if let Some(dd) = ddec.as_mut() {
                                        if *start != data_end {
                                            dd.reset_framer_state();
                                        }
                                        feed_range(dd, &bytes, i0, *start, *end, |_| {});
                                        data_end = *end;
                                    }
                                }
                                Step::Feed { start, end, ctx } => {
                                    let mut ctx = *ctx;
                                    if latched == Some(ctx.call_id) {
                                        ctx.encrypted = true;
                                    }
                                    traffic_reader_imbe.begin_segment(&ctx);
                                    let imbe = &traffic_reader_imbe;
                                    feed_range(&mut dec, &bytes, i0, *start, *end, |last| {
                                        let ms = view
                                            .time_of(last)
                                            .map(|t| unix_ms_of(t, offset))
                                            .unwrap_or(0);
                                        imbe.set_segment_air_ms(ms);
                                    });
                                    let enc = traffic_reader_imbe.end_segment();
                                    if enc && !ctx.encrypted {
                                        latched = Some(ctx.call_id);
                                    }
                                }
                                Step::Discard { .. } | Step::Applied { .. } => {}
                            }
                        }
                    }
                    if let Some(cid) = latched {
                        if state.ctx.call_id == cid {
                            state.ctx.encrypted = true;
                        }
                    }
                    rd.shared.record_steps(&steps, unix_ms_now(), &view, offset);
                }
            }

            let log_every = if mode == DeliveryMode::Legacy { 16 } else { 256 };
            if wakeups <= 5 || wakeups % log_every == 0 {
                let total_dibits: u64 = hist.iter().sum();
                let pct = |v: u64| -> f64 {
                    if total_dibits == 0 { 0.0 } else { 100.0 * v as f64 / total_dibits as f64 }
                };
                let (sync_hits, msg_count, ldu1, ldu2, hdu, tdu, tdu_lc) = {
                    let d = traffic_lsm_decoder_task.read().await;
                    (
                        d.sync_hits(),
                        d.recent_messages.len(),
                        d.ldu1_count,
                        d.ldu2_count,
                        d.hdu_count,
                        d.tdu_count,
                        d.tdu_lc_count,
                    )
                };
                tracing::info!(
                    target: "p25_traffic_lsm",
                    "wake #{wakeups} ({}): cum bytes={total_bytes} \
                     hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}% \
                     | traffic_lsm decoder: sync_hits={sync_hits} \
                     hdu={hdu} ldu1={ldu1} ldu2={ldu2} tdu={tdu} \
                     tdu_lc={tdu_lc} recent_msgs={msg_count}",
                    mode.as_str(),
                    pct(hist[0]), pct(hist[1]), pct(hist[2]), pct(hist[3]),
                );
            }
            rd.maybe_summary();
        }
    });
}
