//! A programme played from its transport stream: the demultiplexer, then each stream's decoder.
//! Out comes sound, two channels at the stream's rate, each syncframe's samples with their
//! presentation time.
//!
//! A PES packet's PTS belongs to the first syncframe that starts in it (H.222.0 2.4.3.7); the
//! syncframes after it take the last time plus their duration. A syncframe that fails to decode
//! comes out as silence of its length, so the sound keeps its time.

use std::collections::VecDeque;

use crate::ac3::{self, frame_len, Decoder, FRAME_SAMPLES};
use crate::ts::{Demux, Program, STREAM_AC3};

/// PTS ticks a second.
pub const PTS_HZ: u64 = 90_000;
const PTS_WRAP: u64 = 1 << 33;

/// One syncframe's sound: two channels interleaved (left, right).
#[derive(Debug, Clone)]
pub struct Sound {
    pub pts: Option<u64>,
    pub rate: u32,
    pub samples: Vec<f32>,
    /// It failed to decode: silence in its place.
    pub lost: bool,
}

/// The sound's format, from its last syncframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub pid: u16,
    pub rate: u32,
    pub acmod: u8,
    pub lfe: bool,
    pub kbps: u32,
}

pub struct Player {
    demux: Demux,
    program: Option<u16>,
    audio_pid: Option<u16>,
    ac3: Decoder,
    /// Audio bytes not yet decoded, and where in them each PES packet with a PTS began.
    es: Vec<u8>,
    stamps: VecDeque<(usize, u64)>,
    /// The next syncframe's time when no PES packet gives it.
    next_pts: Option<u64>,
    sound: VecDeque<Sound>,
    format: Option<AudioFormat>,
    /// Syncframes that failed to decode.
    pub lost_frames: u32,
}

impl Player {
    /// Plays `program`, or the first programme with sound the PAT lists.
    pub fn new(program: Option<u16>) -> Player {
        let mut demux = Demux::default();
        demux.follow(program);
        Player {
            demux,
            program,
            audio_pid: None,
            ac3: Decoder::default(),
            es: Vec::new(),
            stamps: VecDeque::new(),
            next_pts: None,
            sound: VecDeque::new(),
            format: None,
            lost_frames: 0,
        }
    }

    pub fn programs(&self) -> impl Iterator<Item = &Program> {
        self.demux.programs()
    }

    pub fn audio_format(&self) -> Option<AudioFormat> {
        self.format
    }

    /// Any run of the stream's bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.demux.push(bytes);
        if self.audio_pid.is_none() {
            self.choose();
        }
        while let Some(pes) = self.demux.next_pes() {
            if Some(pes.pid) == self.audio_pid {
                if let Some(pts) = pes.pts {
                    self.stamps.push_back((self.es.len(), pts));
                }
                self.es.extend_from_slice(&pes.data);
            }
        }
        self.decode_audio();
    }

    /// The next syncframe's sound.
    pub fn sound(&mut self) -> Option<Sound> {
        self.sound.pop_front()
    }

    /// The programme's main sound: its first AC-3 stream.
    fn choose(&mut self) {
        let program = self.demux.programs().find(|p| !matches!(self.program, Some(n) if n != p.number) && p.streams.iter().any(|s| s.stream_type == STREAM_AC3));
        if let Some(p) = program {
            self.audio_pid = p.streams.iter().find(|s| s.stream_type == STREAM_AC3).map(|s| s.pid);
            let number = p.number;
            if self.program.is_none() {
                self.program = Some(number);
                self.demux.follow(Some(number));
            }
        }
    }

    fn decode_audio(&mut self) {
        let mut at = 0;
        while at + 5 <= self.es.len() {
            let Some(len) = frame_len(&self.es[at..]) else {
                at += 1;
                continue;
            };
            if at + len > self.es.len() {
                break;
            }
            // The PTS of the last PES packet that began by this syncframe's start.
            let mut pts = None;
            while self.stamps.front().is_some_and(|&(o, _)| o <= at) {
                pts = self.stamps.pop_front().map(|(_, t)| t);
            }
            let pts = pts.or(self.next_pts);
            let (samples, rate, lost) = match self.ac3.decode(&self.es[at..at + len]) {
                Ok(f) => {
                    self.format = Some(AudioFormat { pid: self.audio_pid.unwrap_or(0), rate: f.sample_rate, acmod: f.acmod, lfe: f.lfe, kbps: f.bitrate_kbps });
                    (f.stereo().into_iter().flatten().collect(), f.sample_rate, false)
                }
                Err(_) => {
                    self.lost_frames += 1;
                    let rate = self.format.map_or(48_000, |f| f.rate);
                    (vec![0.0; 2 * FRAME_SAMPLES], rate, true)
                }
            };
            self.next_pts = pts.map(|t| (t + FRAME_SAMPLES as u64 * PTS_HZ / u64::from(rate)) % PTS_WRAP);
            self.sound.push_back(Sound { pts, rate, samples, lost });
            at += len;
        }
        // Keep what is not yet a whole syncframe.
        self.es.drain(..at);
        for s in self.stamps.iter_mut() {
            s.0 = s.0.saturating_sub(at);
        }
    }
}

/// What the player tells its page: the programmes and the sound's format, as JSON.
pub fn describe(p: &Player) -> String {
    let programs: Vec<String> = p
        .programs()
        .map(|g| {
            let streams: Vec<String> = g.streams.iter().map(|s| format!("{{\"pid\":{},\"type\":{}}}", s.pid, s.stream_type)).collect();
            format!("{{\"number\":{},\"pcr_pid\":{},\"streams\":[{}]}}", g.number, g.pcr_pid, streams.join(","))
        })
        .collect();
    let audio = match p.audio_format() {
        Some(a) => format!(
            "{{\"pid\":{},\"rate\":{},\"channels\":\"{}\",\"kbps\":{}}}",
            a.pid,
            a.rate,
            ac3::channels(a.acmod, a.lfe),
            a.kbps
        ),
        None => "null".into(),
    };
    let program = p.program.map_or("null".into(), |n| n.to_string());
    format!("{{\"program\":{program},\"programs\":[{}],\"audio\":{audio},\"lost_frames\":{}}}", programs.join(","), p.lost_frames)
}
