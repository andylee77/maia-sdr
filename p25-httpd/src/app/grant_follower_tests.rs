//! Host tests for the portable call lifecycle in `app::grant_follower`
//! (change 056: voice accounting mirrored into `ActiveCallSnapshot`).
//! Attached via `#[cfg(test)] #[path = "grant_follower_tests.rs"] mod tests;`.

use super::*;
use crate::audio::{CallBoundary, CallBoundaryKind};

fn forwarder() -> Arc<ImbeForwarder> {
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    Arc::new(ImbeForwarder::new(tx))
}

fn grant(tg: u16, source: u32, freq_hz: u64, not_followed: Option<&'static str>) -> CallBoundary {
    CallBoundary {
        kind: CallBoundaryKind::CcGrantArrival {
            tg,
            source: Some(source),
            freq_hz: Some(freq_hz),
            channel: 1117,
            encrypted: false,
            not_followed,
        },
        nac: 0,
        talkgroup: Some(tg),
        expected_submit_count: 0,
    }
}

fn chunk(tg: u16, call_id: u64, airtime: bool) -> AudioChunk {
    AudioChunk {
        pcm: [0; 160],
        talkgroup: tg,
        source: 0,
        call_id,
        captured_at_ms: now_unix_ms(),
        airtime,
    }
}

struct Rig {
    active: Option<ActiveCall>,
    next_id: u64,
    tx: CallTrackerEventTx,
    fwd: Arc<ImbeForwarder>,
    dedup: std::collections::HashMap<(u16, u32, Option<u64>, bool), u64>,
    shared: ActiveCallShared,
}

impl Rig {
    fn new() -> Self {
        Rig {
            active: None,
            next_id: 1,
            tx: new_event_tx(),
            fwd: forwarder(),
            dedup: Default::default(),
            shared: new_active_call_shared(),
        }
    }
    fn boundary(&mut self, b: CallBoundary) {
        handle_boundary(b, &mut self.active, &mut self.next_id, &self.tx, &self.fwd, &mut self.dedup);
        mirror_active(&self.active, &self.shared, &self.fwd);
    }
    fn audio(&mut self, c: AudioChunk) {
        handle_audio(c, &mut self.active);
        mirror_active(&self.active, &self.shared, &self.fwd);
    }
    fn snap(&self) -> Option<ActiveCallSnapshot> {
        self.shared.lock().unwrap().clone()
    }
}

#[test]
fn snapshot_counts_voice_of_the_open_call() {
    let mut r = Rig::new();
    let t0 = now_unix_ms();
    r.boundary(grant(300, 3436046, 857_987_500, None));
    let s = r.snap().expect("followed grant opens a call");
    assert_eq!((s.tg, s.source, s.voice_frames), (300, Some(3436046), 0));
    assert_eq!(s.sources_observed, vec![3436046]);
    assert!(s.first_voice_unix_ms.is_none() && s.last_voice_unix_ms.is_none());
    assert!(s.last_activity_unix_ms >= t0, "the opening grant is a keep-alive");

    let id = s.call_id;
    for _ in 0..9 {
        r.audio(chunk(300, id, true));
    }
    let s = r.snap().unwrap();
    assert_eq!(s.voice_frames, 9);
    assert!(s.first_voice_unix_ms.is_some());
    assert!(s.last_voice_unix_ms >= s.first_voice_unix_ms);
    assert!(s.last_activity_unix_ms >= s.last_voice_unix_ms.unwrap());
}

#[test]
fn airtime_tail_of_another_call_is_not_voice_of_this_one() {
    let mut r = Rig::new();
    r.boundary(grant(300, 3436046, 857_987_500, None));
    let first = r.snap().unwrap().call_id;
    // Same TG, next speaker: a new call (grant = call).
    r.boundary(grant(300, 1014, 857_987_500, None));
    let s = r.snap().unwrap();
    assert_ne!(s.call_id, first);
    assert_eq!(s.source, Some(1014));
    // In-flight frames of the previous call decoded after the switch.
    r.audio(chunk(300, first, true));
    r.audio(chunk(300, first, true));
    assert_eq!(r.snap().unwrap().voice_frames, 0);
    r.audio(chunk(300, s.call_id, true));
    assert_eq!(r.snap().unwrap().voice_frames, 1);
}

#[test]
fn hdu_refreshes_keepalive_but_is_not_voice() {
    let mut r = Rig::new();
    r.boundary(grant(300, 3406028, 858_437_500, None));
    r.boundary(CallBoundary {
        kind: CallBoundaryKind::HduStart,
        nac: 0x8A1,
        talkgroup: Some(300),
        expected_submit_count: 0,
    });
    let s = r.snap().unwrap();
    assert_eq!(s.voice_frames, 0);
    assert!(s.last_voice_unix_ms.is_none());
    assert_eq!(s.nac, 0x8A1);
}

#[test]
fn not_followed_grant_never_becomes_the_current_call() {
    let mut r = Rig::new();
    let mut rx = r.tx.subscribe();
    r.boundary(grant(402, 3400015, 858_437_500, Some("encrypted")));
    assert!(r.snap().is_none());
    // Synthetic open + close pair for the call list.
    let a = rx.try_recv().unwrap();
    let b = rx.try_recv().unwrap();
    assert!(matches!(a.kind, CallTrackerEventKind::CallOpen { .. }));
    assert!(matches!(b.kind, CallTrackerEventKind::CallClose { .. }));
    assert_eq!(a.call_id, b.call_id);
}
