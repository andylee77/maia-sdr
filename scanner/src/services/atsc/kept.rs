//! The last TV scan that finished, kept with the unit's state (`state/tv_scan.json`) so the
//! channels it named outlast a restart of the scanner. The windows' spectra are not kept: a
//! channel's spectrum comes with the next scan.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{AtscScan, FoundChannel};
use crate::protocol::atsc::Channel;
use crate::services::config::{self, Stored, VERSION};

/// A finished scan as it is written: what `AtscScan` holds of it once it is over.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KeptScan {
    pub version: u32,
    pub id: u64,
    pub started_unix_ms: u64,
    pub finished_unix_ms: u64,
    /// The channels asked for.
    pub channels: Vec<u8>,
    /// Manual gain, dB; none: the AGC.
    pub gain_db: Option<i32>,
    pub found: Vec<FoundChannel>,
}

impl KeptScan {
    pub fn of(scan: &AtscScan) -> Self {
        KeptScan {
            version: VERSION,
            id: scan.id,
            started_unix_ms: scan.started_unix_ms,
            finished_unix_ms: scan.finished_unix_ms,
            channels: scan.channels.clone(),
            gain_db: scan.gain_db,
            found: scan.found.clone(),
        }
    }

    /// The scan as it stood when it finished; none when no scan was kept. A channel the plan no
    /// longer has is left out.
    pub fn scan(self) -> Option<AtscScan> {
        if self.finished_unix_ms == 0 {
            return None;
        }
        let found = self
            .found
            .into_iter()
            .filter_map(|mut c| {
                c.band = Channel::get(c.number)?.band;
                Some(c)
            })
            .collect();
        Some(AtscScan {
            id: self.id,
            state: "done",
            started_unix_ms: self.started_unix_ms,
            finished_unix_ms: self.finished_unix_ms,
            channels: self.channels,
            gain_db: self.gain_db,
            found,
            ..Default::default()
        })
    }
}

/// The scan kept at `path`, and whether this binary may write the file (not one a newer binary
/// wrote). A missing file, or one that does not read, is no scan.
pub fn load(path: &Path) -> (Option<AtscScan>, bool) {
    match config::load::<KeptScan>(path) {
        Ok(stored) => (stored.value.scan(), stored.writable),
        Err(e) => {
            tracing::warn!("the kept TV scan was not read: {e:#}");
            (None, true)
        }
    }
}

pub fn save(path: &Path, scan: &AtscScan) -> anyhow::Result<()> {
    config::save(path, &Stored { value: KeptScan::of(scan), writable: true })
}

/// Forget the kept scan (a factory reset).
pub fn forget(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("the kept TV scan was not deleted: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::atsc::psip::VirtualChannel;
    use crate::protocol::atsc::spectrum::Kind;
    use crate::protocol::atsc::streams::{AudioFormat, Program, Stream, VideoFormat};
    use crate::services::atsc::Station;

    fn channel(number: u8, station: Option<Station>) -> FoundChannel {
        let plan = Channel::get(number).unwrap();
        FoundChannel {
            number,
            band: plan.band,
            center_hz: plan.center_hz(),
            kind: Kind::Vsb,
            pilot_hz: Some(500_309_443),
            pilot_offset_hz: Some(3),
            pilot_db: Some(20.4),
            level_db: 29.6,
            power_dbm: Some(-36.5),
            gain_db: Some(49.0),
            clips_ppm: Some(0.1),
            station,
        }
    }

    fn wjax() -> Station {
        Station {
            tsid: Some(605),
            channels: vec![VirtualChannel {
                major: 47,
                minor: 1,
                short_name: "WJAXHD".into(),
                long_name: None,
                program: 3,
                service_type: 2,
                source_id: 3,
                hidden: false,
                access_controlled: false,
            }],
            time_unix: Some(1_794_000_000),
            mer_db: Some(26.8),
            packets: 6_189,
            corrected: 0,
            failed: 0,
            programs: vec![Program {
                number: 3,
                pmt_pid: 48,
                pcr_pid: Some(49),
                bitrate_bps: 10_102_106,
                streams: vec![
                    Stream {
                        pid: 49,
                        stream_type: 2,
                        kind: "video",
                        codec: "MPEG-2".into(),
                        language: None,
                        bitrate_bps: 9_513_025,
                        video: Some(VideoFormat {
                            width: 1920,
                            height: 1080,
                            progressive: Some(false),
                            frame_rate: Some(29.97),
                            aspect: Some("16:9"),
                            profile: Some("Main@High".into()),
                        }),
                        audio: None,
                    },
                    Stream {
                        pid: 52,
                        stream_type: 129,
                        kind: "audio",
                        codec: "AC-3".into(),
                        language: Some("eng".into()),
                        bitrate_bps: 391_676,
                        video: None,
                        audio: Some(AudioFormat { channels: "5.1".into(), sample_rate_hz: 48_000, bitrate_bps: 384_000 }),
                    },
                ],
            }],
            null_bps: 120_000,
            error: None,
        }
    }

    fn done() -> AtscScan {
        AtscScan {
            id: 4,
            state: "done",
            started_unix_ms: 1_794_358_000_000,
            finished_unix_ms: 1_794_358_068_000,
            channels: vec![19, 20],
            gain_db: None,
            found: vec![channel(19, Some(wjax())), channel(20, None), channel(9, Some(Station { error: Some("no segment or field syncs".into()), ..Default::default() }))],
            ..Default::default()
        }
    }

    #[test]
    fn a_kept_scan_reads_back_as_it_finished() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("tv_scan.json");
        assert!(load(&path).0.is_none(), "nothing kept yet");
        let scan = done();
        save(&path, &scan).unwrap();
        let (back, writable) = load(&path);
        let back = back.unwrap();
        assert!(writable);
        assert_eq!((back.id, back.state, back.started_unix_ms, back.finished_unix_ms), (4, "done", scan.started_unix_ms, scan.finished_unix_ms));
        assert_eq!((back.channels, back.gain_db), (vec![19, 20], None));
        // Every channel with its band from the plan, its station, its streams' kinds and aspect.
        assert_eq!(back.found, scan.found);
        assert_eq!(back.found[2].band, "VHF high");
        forget(&path);
        assert!(load(&path).0.is_none());
        forget(&path);
    }

    #[test]
    fn what_does_not_read_is_no_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tv_scan.json");
        // A scan that never finished, a channel outside the plan, a kind no stream has.
        assert!(KeptScan { found: vec![channel(19, None)], ..Default::default() }.scan().is_none());
        let mut kept = KeptScan::of(&done());
        kept.found[1].number = 99;
        assert_eq!(kept.scan().unwrap().found.iter().map(|c| c.number).collect::<Vec<_>>(), [19, 9]);
        let text = serde_json::to_string(&KeptScan::of(&done())).unwrap().replace("\"kind\":\"video\"", "\"kind\":\"smell\"");
        std::fs::write(&path, text).unwrap();
        assert!(load(&path).0.is_none(), "moved aside");
        assert!(!path.exists());
    }
}
