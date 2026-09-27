//! Host tests for `audio::rec_storage` (change 057: RAM / SD recording
//! stores, the SD writer thread and per-store retention). Attached via
//! `#[cfg(test)] #[path = "rec_storage_tests.rs"] mod tests;`.
//!
//! The "SD card" is a temp directory; no mount check (`sd_mount: None`).

use super::*;

fn dirs(tag: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("p25_rec_storage_{}_{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let ram = root.join("ram");
    let sd = root.join("sd");
    std::fs::create_dir_all(&ram).unwrap();
    std::fs::create_dir_all(&sd).unwrap();
    (ram, sd)
}

fn cfg(ram: &Path, sd: &Path) -> StorageConfig {
    StorageConfig {
        ram_dir: ram.to_path_buf(),
        sd_dir: sd.to_path_buf(),
        sd_mount: None,
        probe_interval: Duration::from_secs(30),
        max_queue_bytes: MAX_QUEUE_BYTES,
        min_free_bytes: 0,
    }
}

fn entry(id: u64, storage: &'static str, size: u64) -> RecordingEntry {
    RecordingEntry {
        id,
        talkgroup: 300,
        size_bytes: size,
        filename: format!("rec_{}_{id}_tg300.wav", id * 1000),
        storage,
        ..Default::default()
    }
}

fn store_of(entries: Vec<RecordingEntry>) -> RecordingStore {
    Arc::new(tokio::sync::Mutex::new(entries.into_iter().collect()))
}

/// Poll `f` for up to 5 s.
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn filenames_parse_back() {
    assert_eq!(
        parse_filename("rec_5614677_257_tg300_from1014.wav"),
        Some((5_614_677, 257, 300, Some(1014)))
    );
    assert_eq!(parse_filename("rec_1790470523000_12_tg402.wav"), Some((1_790_470_523_000, 12, 402, None)));
    for bad in ["rec_1_2_tg3_from4.mp3", "x_1_2_tg3.wav", "rec_1_2_300.wav", "rec_1_2_tg3_from4_x.wav", ".rec_1_2_tg3.wav.part"] {
        assert!(parse_filename(bad).is_none(), "{bad}");
    }
}

#[test]
fn retention_is_per_store_and_sd_has_a_size_cap() {
    let r = Retention { ram_max_count: 2, sd_max_count: 3, sd_max_bytes: 250 };
    // Oldest first: RAM 1, SD 2 (100 B), RAM 3, SD 4 (100 B), SD 5 (100 B), RAM 6.
    let ring: VecDeque<RecordingEntry> = vec![
        entry(1, STORE_RAM, 10),
        entry(2, STORE_SD, 100),
        entry(3, STORE_RAM, 10),
        entry(4, STORE_SD, 100),
        entry(5, STORE_SD, 100),
        entry(6, STORE_RAM, 10),
    ]
    .into();
    // RAM keeps 3 and 6; SD keeps 5 and 4 (200 B), 2 would make 300 B.
    let gone: Vec<u64> = evictions(&ring, &r).into_iter().map(|i| ring[i].id).collect();
    assert_eq!(gone, vec![1, 2]);
    // A lowered RAM limit never deletes SD recordings.
    let r1 = Retention { ram_max_count: 1, ..r };
    let gone: Vec<u64> = evictions(&ring, &r1).into_iter().map(|i| ring[i].id).collect();
    assert_eq!(gone, vec![1, 2, 3]);
    // The newest recording of a store is kept even when it alone
    // exceeds the size cap.
    let tiny = Retention { sd_max_bytes: 1, ..r };
    let gone: Vec<u64> = evictions(&ring, &tiny).into_iter().map(|i| ring[i].id).collect();
    assert_eq!(gone, vec![1, 2, 4]);
    assert_eq!(usage(&ring), ((3, 30), (3, 300)));
}

#[test]
fn index_lists_sd_recordings_sorted_with_duration() {
    let (ram, sd) = dirs("index");
    let wav = |name: &str, ms: u64| std::fs::write(sd.join(name), vec![0u8; 44 + 16 * ms as usize]).unwrap();
    wav("rec_2000_12_tg300_from1014.wav", 1_440);
    wav("rec_1000_7_tg300_from3436046.wav", 1_620);
    wav("rec_3000_12_tg300.wav", 100); // same id, newer (restart without index)
    std::fs::write(sd.join(".rec_4000_13_tg300.wav.part"), b"half").unwrap();
    std::fs::write(sd.join("notes.txt"), b"x").unwrap();
    let (list, note) = index_sd(&cfg(&ram, &sd));
    let ids: Vec<u64> = list.iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![7, 12], "{note}");
    assert_eq!(list[0].duration_ms, 1_620);
    assert_eq!((list[0].talkgroup, list[0].source), (300, Some(3436046)));
    assert_eq!(list[1].started_unix_ms, 3_000, "newer of the duplicate id kept");
    assert!(list.iter().all(|e| e.storage == STORE_SD && e.pending.is_none()));
    assert!(note.contains("indexed 2"));
}

#[test]
fn sd_write_lands_then_drops_the_ram_copy() {
    let (ram, sd) = dirs("write");
    let bytes = Arc::new(vec![7u8; 44 + 16 * 1_440]);
    let mut e = entry(21, STORE_SD, bytes.len() as u64);
    let path = sd.join(&e.filename);
    e.path = path.clone();
    e.pending = Some(crate::audio::recorder::PendingWav(bytes.clone()));
    let store = store_of(vec![e]);
    let st = RecordingStorage::start(cfg(&ram, &sd), store.clone());
    wait_for("first probe", || st.sd_state() == "ok");
    assert!(st.sd_ready().is_ok());
    st.submit_sd_write(21, path.clone(), bytes.clone());
    wait_for("write", || store.blocking_lock()[0].pending.is_none());
    assert_eq!(std::fs::read(&path).unwrap(), *bytes);
    let s = st.sd_status();
    assert_eq!(s["writes_ok"], 1);
    assert_eq!(s["queue_bytes"], 0);
    assert!(!sd.join(format!(".{}.part", store.blocking_lock()[0].filename)).exists());

    // Retention delete goes through the queue too.
    let gone = store.blocking_lock()[0].clone();
    st.remove(&gone);
    wait_for("delete", || !path.exists());
}

#[test]
fn failed_sd_write_falls_back_to_ram() {
    let (ram, sd) = dirs("fallback");
    let bytes = Arc::new(vec![1u8; 1_000]);
    let mut e = entry(5, STORE_SD, 1_000);
    // A directory that does not exist on the "card": the create fails.
    let path = sd.join("gone").join(&e.filename);
    e.path = path.clone();
    e.pending = Some(crate::audio::recorder::PendingWav(bytes.clone()));
    let store = store_of(vec![e]);
    let st = RecordingStorage::start(cfg(&ram, &sd), store.clone());
    wait_for("first probe", || st.sd_state() == "ok");
    st.submit_sd_write(5, path, bytes.clone());
    wait_for("fallback", || store.blocking_lock()[0].storage == STORE_RAM);
    let e = store.blocking_lock()[0].clone();
    assert!(e.pending.is_none());
    assert_eq!(e.path, ram.join(&e.filename));
    assert_eq!(std::fs::read(&e.path).unwrap(), *bytes);
    let s = st.sd_status();
    assert_eq!((s["writes_failed"].as_u64(), s["fallbacks_to_ram"].as_u64()), (Some(1), Some(1)));
    assert!(s["last_error"].as_str().unwrap().contains("gone"));
}

#[test]
fn stalled_writer_never_blocks_and_new_recordings_go_to_ram_when_full() {
    let (ram, sd) = dirs("stall");
    let wav = 44 + 16 * 1_000;
    let mut c = cfg(&ram, &sd);
    c.max_queue_bytes = 3 * wav as u64;
    let entries: Vec<RecordingEntry> = (1..=4u64)
        .map(|id| {
            let mut e = entry(id, STORE_SD, wav as u64);
            e.path = sd.join(&e.filename);
            e
        })
        .collect();
    let store = store_of(entries);
    let st = RecordingStorage::start(c, store.clone());
    wait_for("first probe", || st.sd_state() == "ok");
    // Stall the writer: it cannot publish its first result while the
    // ring is held (like a multi-second fsync on the card).
    let guard = store.blocking_lock();
    let t0 = Instant::now();
    for id in 1..=3u64 {
        assert!(st.sd_ready().is_ok(), "queue has room for #{id}");
        st.submit_sd_write(id, sd.join(format!("rec_{}_{id}_tg300.wav", id * 1000)), Arc::new(vec![0u8; wav]));
    }
    assert!(t0.elapsed() < Duration::from_millis(200), "submitting never waits for the card");
    // Queue full: the recorder must save the next one to RAM.
    let why = st.sd_ready().unwrap_err();
    assert!(why.contains("queue full"), "{why}");
    let s = st.sd_status();
    assert_eq!(s["queue_jobs"], 3);
    drop(guard);
    wait_for("drain", || st.sd_status()["queue_bytes"] == 0);
    assert!(st.sd_ready().is_ok());
    assert_eq!(st.sd_status()["writes_ok"], 3);
}

#[test]
fn unusable_card_is_reported_without_a_writer_touching_it() {
    let (ram, sd) = dirs("absent");
    // No writer thread: never queue to the card.
    let st = RecordingStorage::new(cfg(&ram, &sd));
    assert!(st.sd_ready().unwrap_err().contains("not running"));
    // The card's directory cannot be created (a file is in the way).
    let blocked = sd.join("file");
    std::fs::write(&blocked, b"x").unwrap();
    let (state, detail, _, _) = probe_sd(&cfg(&ram, &blocked.join("p25_recordings")));
    assert_eq!(state, "error");
    assert!(detail.unwrap().contains("mkdir"));
    // Full: free space below the floor (host statvfs is not wired, so
    // exercise the rule with a floor the check cannot pass on Linux).
    if cfg!(target_os = "linux") {
        let mut c = cfg(&ram, &sd);
        c.min_free_bytes = u64::MAX;
        assert_eq!(probe_sd(&c).0, "full");
    }
}
