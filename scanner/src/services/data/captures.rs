//! The IQ of bursts kept on the card, for the PC's tools (inspectrum, URH, rtl_433). A capture
//! is two files in the folder of its UTC day: `<id>_<centre>M_250k.cs16`, the channel's samples
//! as little-endian 16-bit I and Q, and a SigMF description of the same name
//! (`.sigmf-meta`), which says where and when it was taken and what the burst was. The list is
//! read from the descriptions at start. The card keeps the newest 500 captures or 256 MB of
//! them: the oldest go.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::protocol::ism::Reading;
use crate::services::recordings::storage::write_durable;
use crate::util::time::utc_fields;

/// The folder on the card.
pub const DIR: &str = "data_captures";
const MAX_FILES: usize = 500;
const MAX_BYTES: u64 = 256 << 20;
const DATA: &str = ".cs16";
const META: &str = ".sigmf-meta";
/// The burst as this scanner describes it, among a description's annotations.
const OURS: &str = "fishball:burst";

/// A capture, as the list shows it and as its description keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    /// `<UTC time>_<the burst's number>`.
    pub id: String,
    /// When its first sample was taken.
    pub unix_ms: u64,
    /// The burst: its centre, its width 10 dB down, from its first to its last millisecond on,
    /// the time it was on, its strongest bin over the floor and its level.
    pub freq_hz: u64,
    pub width_hz: u32,
    pub length_ms: u32,
    pub on_ms: u32,
    pub over_db: f32,
    pub level_dbfs: f32,
    /// The samples: their centre and rate, how many, and how many come before the burst.
    pub centre_hz: u64,
    pub sample_rate_hz: u32,
    pub samples: u64,
    pub lead_samples: u32,
    /// The data file's name and size, and the description's name.
    pub file: String,
    pub bytes: u64,
    pub meta: String,
    /// What the burst said, when a decoder read it.
    #[serde(default)]
    pub reading: Option<Reading>,
}

/// A capture to write: what it is, its samples, and the radio that took them.
pub struct Job {
    pub kept: Kept,
    pub iq: Vec<u8>,
    pub radio: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    /// Where they are kept; none on a unit without the folder (a test).
    pub dir: String,
    pub count: usize,
    pub bytes: u64,
    pub max_count: usize,
    pub max_bytes: u64,
    /// Newest first.
    pub items: Vec<Kept>,
}

pub struct Captures {
    dir: PathBuf,
    /// Oldest first.
    list: Mutex<Vec<Kept>>,
}

/// `433.92` for 433,920,000 Hz: the centre as a file's name says it.
fn megahertz(hz: u64) -> String {
    let s = format!("{:.6}", hz as f64 / 1e6);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `20261112T022554Z_0007`: when the burst began (UTC), and its number.
pub fn id_of(unix_ms: u64, seq: u64) -> String {
    let (y, mo, d, h, mi, s) = utc_fields(unix_ms / 1000);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z_{seq:04}")
}

/// `2026-11-12T02:25:54.000Z`, as SigMF writes a time.
fn datetime(unix_ms: u64) -> String {
    let (y, mo, d, h, mi, s) = utc_fields(unix_ms / 1000);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z", unix_ms % 1000)
}

/// The data file's and the description's names.
pub fn names(id: &str, centre_hz: u64, rate_hz: u32) -> (String, String) {
    let stem = format!("{id}_{}M_{}k", megahertz(centre_hz), rate_hz / 1000);
    (format!("{stem}{DATA}"), format!("{stem}{META}"))
}

fn day_of(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// The capture's description: SigMF, with the burst as its one annotation.
fn description(k: &Kept, radio: &str) -> serde_json::Value {
    let low = k.freq_hz as f64 - f64::from(k.width_hz) / 2.0;
    let burst_samples = u64::from(k.length_ms) * u64::from(k.sample_rate_hz) / 1000;
    serde_json::json!({
        "global": {
            "core:datatype": "ci16_le",
            "core:sample_rate": k.sample_rate_hz,
            "core:version": "1.2.0",
            "core:dataset": k.file,
            "core:hw": radio,
            "core:recorder": format!("Fishball scanner {}", crate::boot::version::BUILD_TAG),
            "core:description": "A burst heard in data mode. The ADC's 12 bits are the 16 bits' upper 12: full scale is the ADC's.",
        },
        "captures": [{
            "core:sample_start": 0,
            "core:frequency": k.centre_hz,
            "core:datetime": datetime(k.unix_ms),
        }],
        "annotations": [{
            "core:sample_start": k.lead_samples,
            "core:sample_count": burst_samples,
            "core:freq_lower_edge": low,
            "core:freq_upper_edge": low + f64::from(k.width_hz),
            "core:label": format!("{:.1} dB over the floor", k.over_db),
            OURS: k,
        }],
    })
}

/// The capture a description tells of.
fn read(meta: &Path) -> Option<Kept> {
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(meta).ok()?).ok()?;
    serde_json::from_value(doc.get("annotations")?.get(0)?.get(OURS)?.clone()).ok()
}

impl Captures {
    /// The captures under `sd`, listed from their descriptions (a card with none has no folder
    /// yet).
    pub fn open(sd: &Path) -> Captures {
        let dir = sd.join(DIR);
        let mut list = Vec::new();
        for day in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            for entry in std::fs::read_dir(day.path()).into_iter().flatten().flatten() {
                let path = entry.path();
                if !path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(META)) {
                    continue;
                }
                // A description whose samples are gone is no capture.
                if let Some(k) = read(&path).filter(|k| path.with_file_name(&k.file).is_file()) {
                    list.push(k);
                }
            }
        }
        list.sort_by(|a, b| (a.unix_ms, &a.id).cmp(&(b.unix_ms, &b.id)));
        Captures { dir, list: Mutex::new(list) }
    }

    fn list(&self) -> std::sync::MutexGuard<'_, Vec<Kept>> {
        self.list.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn day_dir(&self, id: &str) -> PathBuf {
        self.dir.join(day_of(id))
    }

    pub fn listing(&self) -> Listing {
        let list = self.list();
        Listing {
            dir: self.dir.display().to_string(),
            count: list.len(),
            bytes: list.iter().map(|k| k.bytes).sum(),
            max_count: MAX_FILES,
            max_bytes: MAX_BYTES,
            items: list.iter().rev().cloned().collect(),
        }
    }

    /// Where one of a capture's two files is, by its name.
    pub fn file(&self, name: &str) -> Option<PathBuf> {
        let list = self.list();
        let k = list.iter().find(|k| k.file == name || k.meta == name)?;
        Some(self.day_dir(&k.id).join(name))
    }

    /// Write a capture's two files and list it; the oldest go when the card holds too many.
    pub fn keep(&self, job: Job) -> std::io::Result<Kept> {
        let mut k = job.kept;
        k.bytes = job.iq.len() as u64;
        let dir = self.day_dir(&k.id);
        std::fs::create_dir_all(&dir)?;
        write_durable(&dir.join(&k.file), &job.iq)?;
        let meta = serde_json::to_vec_pretty(&description(&k, &job.radio)).map_err(std::io::Error::other)?;
        if let Err(e) = write_durable(&dir.join(&k.meta), &meta) {
            let _ = std::fs::remove_file(dir.join(&k.file));
            return Err(e);
        }
        let gone = {
            let mut list = self.list();
            list.push(k.clone());
            let mut gone = Vec::new();
            while list.len() > MAX_FILES || list.iter().map(|k| k.bytes).sum::<u64>() > MAX_BYTES {
                gone.push(list.remove(0));
            }
            gone
        };
        gone.iter().for_each(|k| self.remove_files(k));
        Ok(k)
    }

    fn remove_files(&self, k: &Kept) {
        let dir = self.day_dir(&k.id);
        let _ = std::fs::remove_file(dir.join(&k.file));
        let _ = std::fs::remove_file(dir.join(&k.meta));
        // The day's folder goes with its last capture (it stays when not empty).
        let _ = std::fs::remove_dir(&dir);
    }

    /// Delete one capture; false when there is none of that id.
    pub fn delete(&self, id: &str) -> bool {
        let k = {
            let mut list = self.list();
            let Some(at) = list.iter().position(|k| k.id == id) else { return false };
            list.remove(at)
        };
        self.remove_files(&k);
        true
    }

    /// Delete them all (a factory reset); how many there were.
    pub fn clear(&self) -> usize {
        let all = std::mem::take(&mut *self.list());
        all.iter().for_each(|k| self.remove_files(k));
        let _ = std::fs::remove_dir(&self.dir);
        all.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(unix_ms: u64, seq: u64, samples: usize) -> Job {
        let id = id_of(unix_ms, seq);
        let (file, meta) = names(&id, 433_920_000, 250_000);
        Job {
            kept: Kept {
                id,
                unix_ms,
                freq_hz: 433_880_150,
                width_hz: 11_719,
                length_ms: 392,
                on_ms: 209,
                over_db: 31.2,
                level_dbfs: -30.5,
                centre_hz: 433_920_000,
                sample_rate_hz: 250_000,
                samples: samples as u64,
                lead_samples: 5_000,
                file,
                bytes: 0,
                meta,
                reading: None,
            },
            iq: vec![7; samples * 4],
            radio: "a test".into(),
        }
    }

    // 2026-11-12 02:25:54 UTC.
    const T0: u64 = 1_794_450_354_000;

    #[test]
    fn a_capture_is_two_files_named_for_the_pc_s_tools_in_its_day_s_folder() {
        let sd = tempfile::tempdir().unwrap();
        let captures = Captures::open(sd.path());
        let k = captures.keep(job(T0, 7, 1000)).unwrap();
        assert_eq!(k.id, "20261112T022554Z_0007");
        assert_eq!(k.file, "20261112T022554Z_0007_433.92M_250k.cs16");
        assert_eq!(k.meta, "20261112T022554Z_0007_433.92M_250k.sigmf-meta");
        let dir = sd.path().join(DIR).join("20261112");
        assert_eq!(std::fs::metadata(dir.join(&k.file)).unwrap().len(), 4000);
        assert_eq!(captures.file(&k.file).unwrap(), dir.join(&k.file));
        assert!(captures.file("../../etc/passwd").is_none() && captures.file("other.cs16").is_none());
        // The name gives the tools the centre and the rate.
        assert_eq!(crate::protocol::ism::named_window(&k.file), Some((433_920_000, 250_000)));
        // The description is SigMF: the samples' type, rate, centre and time, and the burst.
        let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join(&k.meta)).unwrap()).unwrap();
        assert_eq!(doc["global"]["core:datatype"], "ci16_le");
        assert_eq!(doc["global"]["core:sample_rate"], 250_000);
        assert_eq!(doc["global"]["core:dataset"], k.file.as_str());
        assert_eq!(doc["captures"][0]["core:frequency"], 433_920_000u64);
        assert_eq!(doc["captures"][0]["core:datetime"], "2026-11-12T02:25:54.000Z");
        assert_eq!(doc["annotations"][0]["core:sample_start"], 5_000);
        assert_eq!(doc["annotations"][0]["core:sample_count"], 98_000);
        assert_eq!(doc["annotations"][0][OURS]["freq_hz"], 433_880_150u64);
    }

    #[test]
    fn the_list_is_read_again_from_the_descriptions() {
        let sd = tempfile::tempdir().unwrap();
        let captures = Captures::open(sd.path());
        let first = captures.keep(job(T0, 1, 100)).unwrap();
        // One with what its burst said.
        let mut read = job(T0 + 86_400_000, 2, 200);
        read.kept.reading = Some(Reading { model: "Oregon Scientific v1".into(), kind: "weather".into(), id: 3, channel: Some(1), temperature_c: Some(14.5), battery_low: Some(false), repeats: 2 });
        let second = captures.keep(read).unwrap();
        assert_eq!(second.reading.as_ref().and_then(|r| r.temperature_c), Some(14.5));
        // A description without its samples is not listed.
        let orphan = captures.keep(job(T0 + 1000, 3, 50)).unwrap();
        std::fs::remove_file(captures.file(&orphan.file).unwrap()).unwrap();
        let again = Captures::open(sd.path()).listing();
        assert_eq!(again.items, vec![second.clone(), first.clone()], "newest first");
        assert_eq!((again.count, again.bytes), (2, 1200));
    }

    #[test]
    fn deleting_takes_both_files_and_the_empty_folder() {
        let sd = tempfile::tempdir().unwrap();
        let captures = Captures::open(sd.path());
        let a = captures.keep(job(T0, 1, 100)).unwrap();
        let b = captures.keep(job(T0 + 2000, 2, 100)).unwrap();
        let dir = sd.path().join(DIR).join("20261112");
        assert!(captures.delete(&a.id) && !captures.delete(&a.id));
        assert!(!dir.join(&a.file).exists() && !dir.join(&a.meta).exists() && dir.join(&b.file).exists());
        assert_eq!(captures.clear(), 1);
        assert!(!sd.path().join(DIR).exists());
        assert_eq!(captures.listing().count, 0);
    }

    #[test]
    fn the_oldest_go_when_the_card_holds_too_much() {
        let sd = tempfile::tempdir().unwrap();
        let captures = Captures::open(sd.path());
        // Three captures of 100 MB would pass the 256 MB kept.
        let big = (100 << 20) / 4;
        let first = captures.keep(job(T0, 1, big)).unwrap();
        captures.keep(job(T0 + 1000, 2, big)).unwrap();
        captures.keep(job(T0 + 2000, 3, big)).unwrap();
        let l = captures.listing();
        assert_eq!(l.count, 2);
        assert!(captures.file(&first.file).is_none() && l.items.iter().all(|k| k.id != first.id));
    }
}
