//! Host tests for `audio::recorder` (change 056: retention and the
//! close-time counter snapshot). Attached via
//! `#[cfg(test)] #[path = "recorder_tests.rs"] mod tests;`.

use super::*;

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
        imbe_extracted: None,
        imbe_dropped: None,
        hdu_count: None,
        ldu1_count: None,
        ldu2_count: None,
        tdu_count: None,
        tdu_lc_count: None,
        vocoder_pcm: None,
        vocoder_errors: None,
        vocoder_silent: None,
        freq_hz: None,
        channel: None,
        chunks_match: None,
        chunks_zero_callid: None,
        chunks_drain: None,
        first_chunk_after_open_ms: None,
        sources_observed: Vec::new(),
        max_chunk_lag_ms: None,
        mean_chunk_lag_ms: None,
    }
}

fn ring_with_files(tag: &str, n: u64) -> (VecDeque<RecordingEntry>, Vec<PathBuf>) {
    let dir = std::env::temp_dir().join(format!("p25_rec_test_{}_{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut ring = VecDeque::new();
    let mut paths = Vec::new();
    for id in 1..=n {
        let p = dir.join(format!("rec_{id}.wav"));
        std::fs::write(&p, b"RIFF").unwrap();
        paths.push(p.clone());
        ring.push_back(entry(id, p));
    }
    (ring, paths)
}

#[test]
fn close_stats_wait_for_vocoder_then_time_out() {
    // Vocoder still behind the close: wait.
    assert!(!close_stats_due(10_000, 500, 490, 10_050));
    // Caught up: snapshot now (before the next call's frames).
    assert!(close_stats_due(10_000, 500, 500, 10_050));
    assert!(close_stats_due(10_000, 500, 520, 10_050));
    // Stuck vocoder: give up after CLOSE_STATS_WAIT_MS.
    assert!(!close_stats_due(10_000, 500, 0, 10_000 + CLOSE_STATS_WAIT_MS - 1));
    assert!(close_stats_due(10_000, 500, 0, 10_000 + CLOSE_STATS_WAIT_MS));
}

#[test]
fn evict_beyond_drops_oldest_and_deletes_wavs() {
    let (mut ring, paths) = ring_with_files("evict", 5);
    let gone = evict_beyond(&mut ring, 3);
    assert_eq!(gone.iter().map(|e| e.id).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(ring.iter().map(|e| e.id).collect::<Vec<_>>(), vec![3, 4, 5]);
    assert!(!paths[0].exists() && !paths[1].exists());
    assert!(paths[2].exists() && paths[4].exists());
    // Never below one entry, and a no-op when within the limit.
    assert!(evict_beyond(&mut ring, 10).is_empty());
    assert_eq!(evict_beyond(&mut ring, 0).len(), 2);
    assert_eq!(ring.len(), 1);
}

#[test]
fn enforce_retention_applies_a_lowered_limit_at_once() {
    let (ring, paths) = ring_with_files("retention", 6);
    let store: RecordingStore = Arc::new(Mutex::new(ring));
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let n = rt.block_on(enforce_retention(&store, 2));
    assert_eq!(n, 4);
    let ids: Vec<u64> = rt.block_on(async { store.lock().await.iter().map(|e| e.id).collect() });
    assert_eq!(ids, vec![5, 6]);
    assert!(!paths[3].exists() && paths[5].exists());
}
