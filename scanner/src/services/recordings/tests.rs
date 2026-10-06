//! The stores, the card writer and retention (the "card" is a temp directory, no mount check),
//! the file names, and the recorder driven by call starts, ends and live audio.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::storage::*;
use super::*;
use crate::audio::alert::{AlertKind, ToneAlert};
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

fn retention(ram_max_count: usize, sd_max_bytes: u64) -> Retention {
    Retention { ram_max_count, sd_keep_days: 30, locked_keep_days: 365, sd_max_bytes, folder_max_count: 4_000 }
}

/// A time the clock counts as valid: 2026-10-06 00:00 UTC.
const NOW: u64 = 1_791_244_800_000;
const DAY: u64 = 86_400_000;

#[test]
fn retention_is_per_store_and_sd_has_a_size_cap() {
    let r = retention(2, 250);
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
    let gone = |ring: &VecDeque<Recording>, r: &Retention| -> Vec<u64> { evictions(ring, r, NOW).into_iter().map(|i| ring[i].id).collect() };
    // RAM keeps 3 and 6; SD keeps 5 and 4 (200 B), 2 would make 300 B.
    assert_eq!(gone(&ring, &r), vec![1, 2]);
    // A lowered RAM limit never deletes SD recordings.
    assert_eq!(gone(&ring, &Retention { ram_max_count: 1, ..r }), vec![1, 2, 3]);
    // The newest recording of a store is kept even when it alone exceeds the size cap.
    let tiny = Retention { sd_max_bytes: 1, ..r };
    assert_eq!(gone(&ring, &tiny), vec![1, 2, 4]);
    assert_eq!(usage(&ring), ((3, 30), (3, 300)));
    // A locked recording counts towards the size but stays; the unlocked ones older go.
    let mut locked = ring.clone();
    locked[3].locked = true;
    assert_eq!(gone(&locked, &tiny), vec![1, 2]);
    assert_eq!(locked_usage(&locked), (1, 100));
}

#[test]
fn the_card_keeps_recordings_for_days_and_locked_ones_longer() {
    let at = |id: u64, days_ago: u64, locked: bool| Recording {
        started_unix_ms: NOW - days_ago * DAY - 1,
        locked,
        ..entry(id, Store::Sd, 10)
    };
    // Oldest first.
    let ring: VecDeque<Recording> = vec![at(1, 400, true), at(2, 366, true), at(3, 364, true), at(4, 31, false), at(5, 30, false), at(6, 29, false), at(7, 0, false)].into();
    let r = retention(40, 1 << 30);
    let gone: Vec<u64> = evictions(&ring, &r, NOW).into_iter().map(|i| ring[i].id).collect();
    assert_eq!(gone, vec![1, 2, 4, 5], "past 365 days locked, past 30 unlocked");
    // With the clock unset (the board starts in 1970) nothing goes for its age.
    assert!(evictions(&ring, &r, 5_000).is_empty());
    // Nor does a recording stamped before the clock was set.
    let stale: VecDeque<Recording> = vec![Recording { started_unix_ms: 9_000, ..entry(1, Store::Sd, 10) }, at(2, 0, false)].into();
    assert!(evictions(&stale, &r, NOW).is_empty());
    // The newest stays whatever its age.
    let lone: VecDeque<Recording> = vec![at(1, 90, false)].into();
    assert!(evictions(&lone, &r, NOW).is_empty());
}

#[test]
fn a_recording_goes_to_its_days_folder_until_the_folder_holds_the_cap() {
    let sd = PathBuf::from("/card/p25_recordings");
    let day = sd.join("2026-10-06");
    let in_folder = |id: u64, folder: &Path| Recording { path: folder.join(format!("rec_{id}.wav")), ..entry(id, Store::Sd, 10) };
    let mut ring: VecDeque<Recording> = (1..=2).map(|id| in_folder(id, &day)).collect();
    assert_eq!(folder_for(&ring, &sd, NOW + 5, false, 3), day);
    ring.push_back(in_folder(3, &day));
    assert_eq!(folder_for(&ring, &sd, NOW + 5, false, 3), sd.join("2026-10-06_2"));
    assert_eq!(folder_for(&ring, &sd, NOW + 5, true, 3), sd.join("locked").join("2026-10-06"), "the locked tree counts its own");
    assert_eq!(folder_for(&ring, &sd, NOW - 1, false, 3), sd.join("2026-10-05"), "the day is the call's, in UTC");
    // RAM recordings do not count.
    ring.iter_mut().for_each(|r| r.store = Store::Ram);
    assert_eq!(folder_for(&ring, &sd, NOW + 5, false, 3), day);
    assert!(index::is_day_folder("2026-10-06") && index::is_day_folder("2026-10-06_12"));
    for bad in ["2026-10-6", "2026-10-06_", "2026-10-06_x", "locked", "20261006", "2026_10_06"] {
        assert!(!index::is_day_folder(bad), "{bad}");
    }
}

#[test]
fn the_index_lists_the_day_folders_and_the_locked_ones_oldest_first_by_name() {
    let (ram, sd) = dirs("index");
    let wav = |folder: &str, name: &str, ms: u64| {
        let f = sd.join(folder);
        std::fs::create_dir_all(&f).unwrap();
        std::fs::write(f.join(name), vec![0u8; 44 + 16 * ms as usize]).unwrap();
    };
    wav("1970-01-01", "rec_2000_12_tg300_from1014.wav", 1_440);
    wav("1970-01-01", "rec_1000_7_tg300_from3436046.wav", 1_620);
    wav("1970-01-01_2", "rec_3000_12_tg300.wav", 100); // same id, newer (ids restarted)
    wav("locked/1970-01-01", "rec_5000_2_tg300.clay.wav", 100); // a low id, but the newest
    wav("1970-01-01", "rec_6000_9_tg87926.clay_electric.wav", 100);
    wav("1970-01-01", ".rec_4000_13_tg300.wav.part", 1);
    wav("1970-01-01", "rec_odd.wav", 1);
    wav("old", "rec_7000_14_tg300.wav", 1); // not a day folder
    std::fs::write(sd.join("notes.txt"), b"x").unwrap();
    std::fs::write(sd.join("rec_8000_15_tg300.wav"), b"x").unwrap(); // the old flat layout
    let (list, note) = index(&cfg(&ram, &sd)).unwrap();
    let ids: Vec<u64> = list.iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![7, 12, 2, 9], "{note}");
    assert!(list.iter().all(|r| r.bytes == 0), "the names only");
    assert_eq!((list[0].tg, list[0].source), (300, Some(3436046)));
    assert_eq!(list[1].started_unix_ms, 3_000, "the newer of the duplicate id is listed");
    assert_eq!(list[1].path, sd.join("1970-01-01_2").join("rec_3000_12_tg300.wav"));
    assert_eq!((list[3].tg, list[3].site.as_str()), (87_926, "clay_electric"));
    assert_eq!(list.iter().map(|e| e.locked).collect::<Vec<_>>(), vec![false, false, true, false], "the locked tree's");
    assert!(list.iter().all(|e| e.store == Store::Sd && e.pending.is_none()));
    assert!(note.contains("indexed 4") && note.contains("1 unrecognised") && note.contains("1 .wav files outside"), "{note}");
}

#[test]
fn reconcile_takes_lengths_from_the_history_and_measures_the_rest() {
    let (ram, sd) = dirs("reconcile");
    // 1.62 s unknown to the history; 0.5 s it knows, with 120 bytes of cue points after the audio.
    let day = sd.join("1970-01-01");
    std::fs::create_dir_all(&day).unwrap();
    std::fs::write(day.join("rec_1000_7_tg300.clay.wav"), vec![0u8; 44 + 16 * 1_620]).unwrap();
    std::fs::write(day.join("rec_2000_8_tg300.clay.wav"), vec![0u8; 44 + 16 * 500 + 120]).unwrap();
    let db = sd.join("history.sqlite");
    let history = HistoryStore::open(&db).unwrap();
    let known = RecordingRow {
        file: "rec_2000_8_tg300.clay.wav".into(),
        store: "sd".into(),
        site: "clay".into(),
        call_id: 8,
        started_ms: 2_000,
        tg: 300,
        source: None,
        bytes: 44 + 16 * 500 + 120,
        duration_ms: 500,
    };
    history.add_recordings(&[known]).unwrap();
    let (mut list, _) = index(&cfg(&ram, &sd)).unwrap();
    assert_eq!(reconcile(&mut list, &history).unwrap(), (1, 0));
    let got: Vec<(u64, u64, u64)> = list.iter().map(|r| (r.id, r.duration_ms, r.bytes)).collect();
    assert_eq!(got, vec![(7, 1_620, 44 + 16 * 1_620), (8, 500, 44 + 16 * 500 + 120)]);
}

#[test]
fn a_card_write_lands_then_drops_the_ram_copy() {
    let (ram, sd) = dirs("write");
    let bytes = Arc::new(vec![7u8; 44 + 16 * 1_440]);
    let mut e = entry(21, Store::Sd, bytes.len() as u64);
    // Its day folder is made with the first write.
    let folder = sd.join("2026-10-06");
    let path = folder.join(&e.file);
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
    assert!(!folder.join(format!(".{}.part", ring.lock().unwrap()[0].file)).exists());

    // Deletes go through the queue too, with the folder its last recording leaves.
    let gone = ring.lock().unwrap()[0].clone();
    st.remove(&gone, true);
    wait_for("delete", || !path.exists() && !folder.exists());
}

#[test]
fn a_failed_card_write_falls_back_to_ram_and_the_card_says_failing_until_one_lands() {
    let (ram, sd) = dirs("fallback");
    let bytes = Arc::new(vec![1u8; 1_000]);
    let mut e = entry(5, Store::Sd, 1_000);
    // A file where its folder would be: the write fails, though the card has room (as a full
    // FAT directory does).
    std::fs::write(sd.join("blocked"), b"x").unwrap();
    let path = sd.join("blocked").join(&e.file);
    e.path = path.clone();
    e.pending = Some(bytes.clone());
    let mut good = entry(6, Store::Sd, 1_000);
    good.path = sd.join("2026-10-06").join(&good.file);
    good.pending = Some(bytes.clone());
    let ring = ring_of(vec![e, good.clone()]);
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
    assert!(s.last_error.unwrap().contains("blocked"));
    // The probe after it finds the card fine, and does not call it ok.
    assert_eq!(st.sd_state(), SdState::Failing);
    assert!(st.sd_status().detail.unwrap().contains("blocked"));
    st.request_probe();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(st.sd_state(), SdState::Failing);
    // New recordings still try the card, and one that lands clears it.
    assert!(st.sd_ready().is_ok());
    st.submit_sd_write(6, good.path.clone(), bytes.clone());
    wait_for("write", || st.sd_state() == SdState::Ok);
    assert!(good.path.exists());
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
    Policy { enabled: true, every_call: true, store, retention: retention(40, 1 << 30) }
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
    std::fs::create_dir_all(sd.join("1970-01-01")).unwrap();
    std::fs::write(sd.join("1970-01-01").join("rec_1000_41_tg300.clay.wav"), vec![0u8; 44 + 1_600]).unwrap();
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
    // In its call's day folder (2026-10-01 UTC).
    let day = sd.join("2026-10-01");
    assert_eq!(rec.get(42).unwrap().path, day.join(&r.file));
    assert_eq!(std::fs::read(day.join(&r.file)).unwrap().len(), 44 + 180 * 16);
    assert!(!r.locked);
    let s = rec.summary();
    assert_eq!((s.counters.saved, s.counters.no_audio, s.sd.count), (1, 1, 2));

    // Locked, it moves to the day's folder in the locked tree, the day folder going with it;
    // unlocked, it comes back.
    let locked = rec.set_locked(42, true).await.unwrap();
    let in_locked = sd.join("locked").join("2026-10-01").join(&r.file);
    assert_eq!((locked.locked, locked.path.clone()), (true, in_locked.clone()));
    assert!(in_locked.exists() && !day.exists());
    assert_eq!(rec.summary().locked.count, 1);
    let back = rec.set_locked(42, false).await.unwrap();
    assert_eq!((back.locked, back.path.clone()), (false, day.join(&r.file)));
    assert!(day.join(&r.file).exists() && !sd.join("locked").join("2026-10-01").exists());
    assert!(matches!(rec.set_locked(99, true).await, Err(LockError::Unknown)));

    assert!(rec.delete(42));
    wait_for("delete", || !day.join(&r.file).exists() && !day.exists());
    assert_eq!(rec.clear(Some(Store::Ram)), 0);
    assert_eq!(rec.clear(None), 1);
    wait_for("clear", || !sd.join("1970-01-01").exists());
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
async fn an_alert_tone_is_a_bookmark_a_history_row_and_a_notice() {
    let (ram, sd) = dirs("alert");
    let c = cfg(&ram, &sd);
    let audio = Audio::start(&[Lane::One]);
    let notices = Notices::default();
    let mut heard = notices.subscribe();
    let rec = Recordings::start(c, policy(Store::Ram), (Vec::new(), String::new()), &audio, Default::default(), notices);
    rec.sender().start(call(9));
    tokio::time::sleep(Duration::from_millis(50)).await;
    // A warble 40 ms in, summarised a little longer on each chunk, then speech.
    let summary = |frames: u32| ToneAlert {
        seq: 3,
        kind: AlertKind::Warble,
        tones_hz: vec![806.5, 1506.0],
        segments: 1 + frames / 12,
        offset_ms: 40,
        duration_ms: frames * 20,
    };
    for i in 0..60u32 {
        let alert = (10..=57).contains(&i).then(|| Arc::new(summary(i)));
        audio.inject(AudioChunk {
            lane: Lane::One,
            call: 9,
            tg: 300,
            source: Some(1014),
            speaker: Side::Both,
            pcm: [0; 160],
            error: false,
            silent: false,
            alert,
        });
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    rec.sender().end(CallEnd { call: 9, source: Some(1014), sources: vec![1014] });
    rec.flush(Duration::from_secs(5)).await;

    let r = rec.get(9).unwrap();
    assert_eq!(r.duration_ms, 1_200);
    assert!(r.locked, "a call with alert tones is kept longer");
    assert_eq!(r.bookmarks.len(), 1);
    let b = &r.bookmarks[0];
    assert_eq!((b.offset_ms, b.duration_ms, b.label.as_str(), b.kind.as_str()), (40, 1_140, "warble 806.5/1506.0 Hz", "warble"));
    let wav = std::fs::read(&r.path).unwrap();
    assert_eq!(wav.len() as u64, r.bytes);
    assert!(wav.windows(4).any(|w| w == b"cue "), "the WAV carries the cue point");
    let alerts = rec.alerts_of(9);
    assert_eq!(alerts.len(), 1);
    let a = &alerts[0];
    assert_eq!((a.call_id, a.tg, a.source, a.lane, a.segments), (9, 300, Some(1014), 1, 5));
    assert!(a.at_ms >= unix_ms() - 10_000 && a.at_ms <= unix_ms());
    let mut got = Vec::new();
    while let Ok(n) = heard.try_recv() {
        got.push(n);
    }
    assert_eq!(got, vec![Notice::RecordingSaved { call: 9 }, Notice::Alert { call: 9, tg: 300 }], "the recording first");
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
