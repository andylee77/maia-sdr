//! The stores, the card writer and retention (the "card" is a temp directory, no mount check),
//! the file names, and the recorder driven by call starts, ends and live audio.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::storage::*;
use super::*;
use crate::audio::live::VoiceBatch;
use crate::protocol::events::VoiceFrames;
use crate::protocol::p25::voice_frame::ImbeFrameRaw;
use crate::services::config::aliases::Side;

fn dirs(tag: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("scanner_recordings_{}_{tag}", std::process::id()));
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

fn entry(id: u64, store: Store, bytes: u64) -> Recording {
    Recording { id, tg: 300, bytes, file: format!("rec_{}_{id}_tg300.wav", id * 1000), store, ..Default::default() }
}

fn ring_of(entries: Vec<Recording>) -> Ring {
    Arc::new(Mutex::new(entries.into_iter().collect()))
}

/// Poll `f` for up to 5 s.
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn parsed(started: u64, call: u64, tg: u32, source: Option<u32>, site: &str) -> Option<index::Parsed> {
    Some(index::Parsed { started_unix_ms: started, call, tg, source, site: site.into() })
}

#[test]
fn file_names_parse_back() {
    assert_eq!(index::parse("rec_5614677_257_tg300_from1014.wav"), parsed(5_614_677, 257, 300, Some(1014), ""));
    assert_eq!(index::parse("rec_1790470523000_12_tg402.wav"), parsed(1_790_470_523_000, 12, 402, None, ""));
    assert_eq!(
        index::parse("rec_1790634553548_581_tg300_from3402099.fpl_clay.wav"),
        parsed(1_790_634_553_548, 581, 300, Some(3_402_099), "fpl_clay")
    );
    assert_eq!(index::parse("rec_1_2_tg3.clay.wav"), parsed(1, 2, 3, None, "clay"));
    // Talkgroups are 32-bit (DMR Tier III's are 24-bit).
    assert_eq!(index::parse("rec_1_2_tg16777215.wav"), parsed(1, 2, 16_777_215, None, ""));
    assert_eq!(
        index::parse("rec_1790634553548_582_tg87921_from1234567.clay_electric.wav"),
        parsed(1_790_634_553_548, 582, 87_921, Some(1_234_567), "clay_electric")
    );
    for bad in [
        "rec_1_2_tg3_from4.mp3", "x_1_2_tg3.wav", "rec_1_2_300.wav", "rec_1_2_tg3_from4_x.wav", ".rec_1_2_tg3.wav.part",
        "rec_1_2_tg3.a.b.wav", "rec_1_2_tg3..wav",
    ] {
        assert!(index::parse(bad).is_none(), "{bad}");
    }
    assert_eq!(index::file_name(1000, 7, 300, Some(1014), "psic_st_johns"), "rec_1000_7_tg300_from1014.psic_st_johns.wav");
    assert_eq!(index::file_name(1000, 7, 300, None, ""), "rec_1000_7_tg300.wav");
    assert_eq!(index::file_name(1000, 7, 300, None, "a/b"), "rec_1000_7_tg300.wav");
}

#[test]
fn retention_is_per_store_and_sd_has_a_size_cap() {
    let r = Retention { ram_max_count: 2, sd_max_count: 3, sd_max_bytes: 250 };
    // Oldest first: RAM 1, SD 2 (100 B), RAM 3, SD 4 (100 B), SD 5 (100 B), RAM 6.
    let ring: VecDeque<Recording> = vec![
        entry(1, Store::Ram, 10),
        entry(2, Store::Sd, 100),
        entry(3, Store::Ram, 10),
        entry(4, Store::Sd, 100),
        entry(5, Store::Sd, 100),
        entry(6, Store::Ram, 10),
    ]
    .into();
    // RAM keeps 3 and 6; SD keeps 5 and 4 (200 B), 2 would make 300 B.
    let gone: Vec<u64> = evictions(&ring, &r).into_iter().map(|i| ring[i].id).collect();
    assert_eq!(gone, vec![1, 2]);
    // A lowered RAM limit never deletes SD recordings.
    let r1 = Retention { ram_max_count: 1, ..r };
    let gone: Vec<u64> = evictions(&ring, &r1).into_iter().map(|i| ring[i].id).collect();
    assert_eq!(gone, vec![1, 2, 3]);
    // The newest recording of a store is kept even when it alone exceeds the size cap.
    let tiny = Retention { sd_max_bytes: 1, ..r };
    let gone: Vec<u64> = evictions(&ring, &tiny).into_iter().map(|i| ring[i].id).collect();
    assert_eq!(gone, vec![1, 2, 4]);
    assert_eq!(usage(&ring), ((3, 30), (3, 300)));
}

#[test]
fn the_index_lists_card_recordings_oldest_first_with_their_length() {
    let (ram, sd) = dirs("index");
    let wav = |name: &str, ms: u64| std::fs::write(sd.join(name), vec![0u8; 44 + 16 * ms as usize]).unwrap();
    wav("rec_2000_12_tg300_from1014.wav", 1_440);
    wav("rec_1000_7_tg300_from3436046.wav", 1_620);
    wav("rec_3000_12_tg300.wav", 100); // same id, newer (ids restarted)
    wav("rec_5000_2_tg300.clay.wav", 100); // a low id, but the newest
    wav("rec_6000_9_tg87926.clay_electric.wav", 100);
    std::fs::write(sd.join(".rec_4000_13_tg300.wav.part"), b"half").unwrap();
    std::fs::write(sd.join("notes.txt"), b"x").unwrap();
    std::fs::write(sd.join("rec_odd.wav"), b"x").unwrap();
    let (list, note) = index(&cfg(&ram, &sd)).unwrap();
    let ids: Vec<u64> = list.iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![7, 12, 2, 9], "{note}");
    assert_eq!(list[0].duration_ms, 1_620);
    assert_eq!((list[0].tg, list[0].source), (300, Some(3436046)));
    assert_eq!(list[1].started_unix_ms, 3_000, "the newer of the duplicate id is listed");
    assert_eq!((list[3].tg, list[3].site.as_str()), (87_926, "clay_electric"));
    assert!(list.iter().all(|e| e.store == Store::Sd && e.pending.is_none()));
    assert!(note.contains("indexed 4") && note.contains("1 unrecognised"), "{note}");
}

#[test]
fn a_card_write_lands_then_drops_the_ram_copy() {
    let (ram, sd) = dirs("write");
    let bytes = Arc::new(vec![7u8; 44 + 16 * 1_440]);
    let mut e = entry(21, Store::Sd, bytes.len() as u64);
    let path = sd.join(&e.file);
    e.path = path.clone();
    e.pending = Some(bytes.clone());
    let ring = ring_of(vec![e]);
    let st = Storage::start(cfg(&ram, &sd), ring.clone());
    wait_for("first probe", || st.sd_state() == SdState::Ok);
    assert!(st.sd_ready().is_ok());
    st.submit_sd_write(21, path.clone(), bytes.clone());
    wait_for("write", || ring.lock().unwrap()[0].pending.is_none());
    assert_eq!(std::fs::read(&path).unwrap(), *bytes);
    let s = st.sd_status();
    assert_eq!((s.writes_ok, s.queue_bytes), (1, 0));
    assert!(!sd.join(format!(".{}.part", ring.lock().unwrap()[0].file)).exists());

    // Deletes go through the queue too.
    let gone = ring.lock().unwrap()[0].clone();
    st.remove(&gone);
    wait_for("delete", || !path.exists());
}

#[test]
fn a_failed_card_write_falls_back_to_ram() {
    let (ram, sd) = dirs("fallback");
    let bytes = Arc::new(vec![1u8; 1_000]);
    let mut e = entry(5, Store::Sd, 1_000);
    // A directory that does not exist on the "card": the create fails.
    let path = sd.join("gone").join(&e.file);
    e.path = path.clone();
    e.pending = Some(bytes.clone());
    let ring = ring_of(vec![e]);
    let st = Storage::start(cfg(&ram, &sd), ring.clone());
    wait_for("first probe", || st.sd_state() == SdState::Ok);
    st.submit_sd_write(5, path, bytes.clone());
    wait_for("fallback", || ring.lock().unwrap()[0].store == Store::Ram);
    let e = ring.lock().unwrap()[0].clone();
    assert!(e.pending.is_none());
    assert_eq!(e.path, ram.join(&e.file));
    assert_eq!(std::fs::read(&e.path).unwrap(), *bytes);
    let s = st.sd_status();
    assert_eq!((s.writes_failed, s.fallbacks_to_ram), (1, 1));
    assert!(s.last_error.unwrap().contains("gone"));
}

#[test]
fn a_stalled_writer_never_blocks_and_new_recordings_go_to_ram_when_its_queue_is_full() {
    let (ram, sd) = dirs("stall");
    let wav = 44 + 16 * 1_000;
    let mut c = cfg(&ram, &sd);
    c.max_queue_bytes = 3 * wav as u64;
    let entries: Vec<Recording> = (1..=4u64)
        .map(|id| {
            let mut e = entry(id, Store::Sd, wav as u64);
            e.path = sd.join(&e.file);
            e
        })
        .collect();
    let ring = ring_of(entries);
    let st = Storage::start(c, ring.clone());
    wait_for("first probe", || st.sd_state() == SdState::Ok);
    // Stall the writer: it cannot publish its first result while the list is held (like a
    // multi-second fsync on the card).
    let guard = ring.lock().unwrap();
    let t0 = Instant::now();
    for id in 1..=3u64 {
        assert!(st.sd_ready().is_ok(), "queue has room for #{id}");
        st.submit_sd_write(id, sd.join(format!("rec_{}_{id}_tg300.wav", id * 1000)), Arc::new(vec![0u8; wav]));
    }
    assert!(t0.elapsed() < Duration::from_millis(200), "submitting never waits for the card");
    let why = st.sd_ready().unwrap_err();
    assert!(why.contains("queue full"), "{why}");
    assert_eq!(st.sd_status().queue_jobs, 3);
    drop(guard);
    wait_for("drain", || st.sd_status().queue_bytes == 0);
    assert!(st.sd_ready().is_ok());
    assert_eq!(st.sd_status().writes_ok, 3);
}

#[test]
fn an_unusable_card_is_reported_without_a_writer_touching_it() {
    let (ram, sd) = dirs("absent");
    let st = Storage::new(cfg(&ram, &sd));
    assert!(st.sd_ready().unwrap_err().contains("not running"));
    // The card's directory cannot be created (a file is in the way).
    let blocked = sd.join("file");
    std::fs::write(&blocked, b"x").unwrap();
    let c = cfg(&ram, &blocked.join("p25_recordings"));
    let st = Storage::start(c, ring_of(Vec::new()));
    wait_for("probe", || st.sd_state() != SdState::Unknown);
    assert_eq!(st.sd_state(), SdState::Error);
    assert!(st.sd_status().detail.unwrap().contains("mkdir"));
    assert!(st.sd_ready().unwrap_err().contains("sd error"));
}

fn policy(store: Store) -> Policy {
    Policy { enabled: true, every_call: true, store, retention: Retention { ram_max_count: 40, sd_max_count: 2_000, sd_max_bytes: 1 << 30 } }
}

fn call(id: u64) -> CallStart {
    CallStart {
        call: id,
        site: "clay".into(),
        tg: 300,
        source: Some(1014),
        lane: Lane::One,
        freq_hz: Some(857_987_500),
        channel: Some("0-1117".into()),
        started_unix_ms: 1_790_870_000_000,
        record: false,
    }
}

/// Play one LDU of voice for `id` and wait until it is out of the pacer.
async fn play(audio: &Audio, id: u64) {
    let mut rx = audio.subscribe();
    let frames = VoiceFrames::Imbe([ImbeFrameRaw { bits: [0x55; 18] }; 9]);
    assert!(audio.voice(VoiceBatch { lane: Lane::One, call: id, tg: 300, source: Some(1014), speaker: Side::Both, frames }));
    for _ in 0..9 {
        tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    }
    // The recorder has taken the chunks too.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn a_followed_call_is_saved_to_the_card_and_ids_continue_past_the_card() {
    let (ram, sd) = dirs("recorder");
    std::fs::write(sd.join("rec_1000_41_tg300.clay.wav"), vec![0u8; 44 + 1_600]).unwrap();
    let c = cfg(&ram, &sd);
    let audio = Audio::start(&[Lane::One]);
    let rec = Recordings::start(c.clone(), policy(Store::Sd), index(&c).unwrap(), &audio, Default::default(), Default::default());
    assert_eq!(rec.next_call(), 42);
    let tx = rec.sender();
    tx.start(call(42));
    play(&audio, 42).await;
    tx.end(CallEnd { call: 42, source: Some(1014), sources: vec![1014, 3412754] });
    // A call with no audio leaves no file.
    tx.start(call(43));
    tx.end(CallEnd { call: 43, source: None, sources: Vec::new() });
    rec.flush(Duration::from_secs(5)).await;

    let (items, total) = rec.list(None, 10);
    assert_eq!((items.iter().map(|r| r.id).collect::<Vec<_>>(), total), (vec![42, 41], 2));
    let r = &items[0];
    assert_eq!(r.file, "rec_1790870000000_42_tg300_from1014.clay.wav");
    assert_eq!((r.store, r.duration_ms, r.lane), (Store::Sd, 180, Some(1)));
    assert_eq!(r.voice.map(|v| v.frames), Some(9));
    assert_eq!(r.sources, vec![1014, 3412754]);
    assert!(rec.get(42).unwrap().pending.is_none(), "on the card");
    assert_eq!(std::fs::read(sd.join(&r.file)).unwrap().len(), 44 + 180 * 16);
    let s = rec.summary();
    assert_eq!((s.counters.saved, s.counters.no_audio, s.sd.count), (1, 1, 2));

    assert!(rec.delete(42));
    wait_for("delete", || !sd.join(&r.file).exists());
    assert_eq!(rec.clear(Some(Store::Ram)), 0);
    assert_eq!(rec.clear(None), 1);
}

#[tokio::test]
async fn with_recording_off_calls_are_counted_not_saved() {
    let (ram, sd) = dirs("off");
    let c = cfg(&ram, &sd);
    let audio = Audio::start(&[Lane::One]);
    let rec = Recordings::start(c, Policy { enabled: false, ..policy(Store::Ram) }, (Vec::new(), String::new()), &audio, Default::default(), Default::default());
    assert_eq!(rec.next_call(), 1);
    rec.sender().start(call(5));
    play(&audio, 5).await;
    rec.flush(Duration::from_secs(5)).await;
    let s = rec.summary();
    assert_eq!((s.counters.saved, s.counters.off, s.ram.count), (0, 1, 0));
    // Turned on again, the next call goes to RAM.
    rec.set_policy(policy(Store::Ram));
    rec.sender().start(call(6));
    play(&audio, 6).await;
    rec.flush(Duration::from_secs(5)).await;
    let r = rec.get(6).unwrap();
    assert_eq!((r.store, r.path.clone()), (Store::Ram, ram.join(&r.file)));
    assert!(r.path.exists());
}

#[tokio::test]
async fn without_every_call_only_the_aliases_that_say_record_are_saved() {
    let (ram, sd) = dirs("aliased");
    let c = cfg(&ram, &sd);
    let audio = Audio::start(&[Lane::One]);
    let rec = Recordings::start(c, Policy { every_call: false, ..policy(Store::Ram) }, (Vec::new(), String::new()), &audio, Default::default(), Default::default());
    rec.sender().start(call(7));
    play(&audio, 7).await;
    rec.sender().start(CallStart { record: true, ..call(8) });
    play(&audio, 8).await;
    rec.flush(Duration::from_secs(5)).await;
    assert!(rec.get(7).is_none(), "its alias does not say record");
    assert!(rec.get(8).is_some());
}
