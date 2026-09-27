//! Host tests for `audio::recorder` (change 056: retention; change 057:
//! per-store retention, SD saving, per-call counters by call_id).
//! Attached via `#[cfg(test)] #[path = "recorder_tests.rs"] mod tests;`.

use super::*;
use crate::audio::rec_storage::StorageConfig;

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("p25_rec_test_{}_{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("ram")).unwrap();
    std::fs::create_dir_all(dir.join("sd")).unwrap();
    dir
}

fn storage(dir: &std::path::Path) -> StorageConfig {
    StorageConfig {
        ram_dir: dir.join("ram"),
        sd_dir: dir.join("sd"),
        sd_mount: None,
        probe_interval: Duration::from_secs(30),
        max_queue_bytes: rec_storage::MAX_QUEUE_BYTES,
        min_free_bytes: 0,
    }
}

fn entry(id: u64, path: PathBuf) -> RecordingEntry {
    RecordingEntry {
        id,
        talkgroup: 300,
        source: Some(1014),
        started_unix_ms: id * 1_000,
        duration_ms: 1_440,
        path,
        size_bytes: 23_084,
        filename: format!("rec_{id}.wav"),
        storage: STORE_RAM,
        ..Default::default()
    }
}

fn ring_with_files(dir: &std::path::Path, n: u64) -> (VecDeque<RecordingEntry>, Vec<PathBuf>) {
    let mut ring = VecDeque::new();
    let mut paths = Vec::new();
    for id in 1..=n {
        let p = dir.join("ram").join(format!("rec_{id}.wav"));
        std::fs::write(&p, b"RIFF").unwrap();
        paths.push(p.clone());
        ring.push_back(entry(id, p));
    }
    (ring, paths)
}

fn ram_only(n: usize) -> Retention {
    Retention { ram_max_count: n, ..Retention::default() }
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A closed call of `frames` 20 ms voice frames.
fn call(id: u64, frames: usize) -> ActiveCall {
    let mut c = ActiveCall::new(id, 300);
    c.source = Some(1014);
    c.pcm = vec![100i16; frames * 160];
    c.chunks_match = frames as u64;
    c.close_at_ms = Some(c.open_at_ms + 2_000);
    c
}

#[test]
fn wav_bytes_have_exact_sizes() {
    let b = wav_bytes(&[1i16, -1, 2]);
    assert_eq!(b.len(), 44 + 6);
    assert_eq!(&b[0..4], b"RIFF");
    assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 36 + 6);
    assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 6);
    assert_eq!(i16::from_le_bytes([b[46], b[47]]), -1);
}

#[test]
fn retention_drops_oldest_and_deletes_wavs() {
    let dir = tmp("evict");
    let st = RecordingStorage::new(storage(&dir));
    let (mut ring, paths) = ring_with_files(&dir, 5);
    let gone = apply_retention(&mut ring, &ram_only(3), &st);
    assert_eq!(gone.iter().map(|e| e.id).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(ring.iter().map(|e| e.id).collect::<Vec<_>>(), vec![3, 4, 5]);
    assert!(!paths[0].exists() && !paths[1].exists());
    assert!(paths[2].exists() && paths[4].exists());
    // Never below one entry, and a no-op when within the limit.
    assert!(apply_retention(&mut ring, &ram_only(10), &st).is_empty());
    assert_eq!(apply_retention(&mut ring, &ram_only(0), &st).len(), 2);
    assert_eq!(ring.len(), 1);
}

#[test]
fn enforce_retention_applies_a_lowered_limit_at_once() {
    let dir = tmp("retention");
    let st = RecordingStorage::new(storage(&dir));
    let (ring, paths) = ring_with_files(&dir, 6);
    let store: RecordingStore = Arc::new(Mutex::new(ring));
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let n = rt.block_on(enforce_retention(&store, &st, ram_only(2)));
    assert_eq!(n, 4);
    let ids: Vec<u64> = rt.block_on(async { store.lock().await.iter().map(|e| e.id).collect() });
    assert_eq!(ids, vec![5, 6]);
    assert!(!paths[3].exists() && paths[5].exists());
}

#[test]
fn counters_are_the_calls_own_frames() {
    // 056 R1 was 144–153 for a 72-frame PTT: global deltas included the
    // next call. Now the recording reads its call_id's counters.
    let dir = tmp("counters");
    let st = RecordingStorage::new(storage(&dir));
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let fwd = Arc::new(crate::app::imbe_forwarder::ImbeForwarder::new(tx));
    fwd.call_counts.update(41, |c| {
        c.imbe_extracted = 72;
        c.ldu1 = 4;
        c.ldu2 = 4;
        c.vocoder_pcm_samples = 72 * 160;
    });
    fwd.call_counts.update(42, |c| c.imbe_extracted = 81);
    let store = new_store();
    let target = SaveTarget { storage: &st, kind: StorageKind::Ram, retention: Retention::default() };
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(finalize(&store, call(41, 72), 41, None, Some(&fwd), None, &target));
    let e = rt.block_on(async { store.lock().await[0].clone() });
    assert_eq!((e.imbe_extracted, e.ldu1_count, e.ldu2_count), (Some(72), Some(4), Some(4)));
    assert_eq!(e.vocoder_pcm, Some(72 * 160));
    assert_eq!((e.duration_ms, e.storage), (1_440, STORE_RAM));
    assert!(e.path.starts_with(dir.join("ram")) && e.path.exists());
}

#[test]
fn sd_recording_is_listed_at_once_and_written_off_the_task() {
    let dir = tmp("sd");
    let store = new_store();
    let st = RecordingStorage::start(storage(&dir), store.clone());
    wait_for("probe", || st.sd_state() == "ok");
    let target = SaveTarget { storage: &st, kind: StorageKind::Sd, retention: Retention::default() };
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(finalize(&store, call(7, 81), 7, None, None, None, &target));
    let e = rt.block_on(async { store.lock().await[0].clone() });
    assert_eq!(e.storage, STORE_SD);
    assert!(e.path.starts_with(dir.join("sd")));
    wait_for("sd write", || store.blocking_lock()[0].pending.is_none());
    assert_eq!(std::fs::metadata(&e.path).unwrap().len(), 44 + 81 * 320);
    // JSON: storage shown, no pending flag once written.
    let j = serde_json::to_value(&store.blocking_lock()[0]).unwrap();
    assert_eq!(j["storage"], "sd");
    assert!(j.get("sd_pending").is_none());
}

#[test]
fn sd_selected_but_unusable_saves_to_ram() {
    let dir = tmp("sd_down");
    let store = new_store();
    // No writer thread = card unusable.
    let st = RecordingStorage::new(storage(&dir));
    let target = SaveTarget { storage: &st, kind: StorageKind::Sd, retention: Retention::default() };
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(finalize(&store, call(9, 10), 9, None, None, None, &target));
    let e = rt.block_on(async { store.lock().await[0].clone() });
    assert_eq!(e.storage, STORE_RAM);
    assert!(e.pending.is_none() && e.path.exists());
    assert_eq!(st.sd_status()["fallbacks_to_ram"], 1);
}
