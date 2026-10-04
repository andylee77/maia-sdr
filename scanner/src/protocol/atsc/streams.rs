//! What a station's transport stream carries, from a capture's packets: each program's elementary
//! streams as its PMT lists them (stream type, PID, language), each stream's share of the
//! multiplex as a bitrate (and the null packets' share: what the station leaves unused), and the
//! format the streams' own headers state. MPEG-2 video's sequence
//! header and extension give its size, scan, frame rate, aspect and profile; an AC-3 sync frame
//! gives its channels, sample rate and bitrate.

use std::collections::HashMap;

use serde::Serialize;

use super::ts::{pid, Sections, PAT_PID};
use super::vsb::{DATA_SEGMENTS, FIELD_SEGMENTS, SEGMENT};
use super::SYMBOL_RATE_HZ;

const PAT: u8 = 0x00;
const PMT: u8 = 0x02;
const ISO_639_LANGUAGE: u8 = 0x0A;
const NULL_PID: u16 = 0x1FFF;
/// The most of a stream's payload kept for its headers: a second of HD video.
const ES_KEEP: usize = 2 << 20;

/// 8-VSB's transport stream rate: 312 packets of 188 bytes a field of 313 segments of 832 symbols
/// (19.39 Mbit/s).
pub fn ts_rate_bps() -> f64 {
    SYMBOL_RATE_HZ / (FIELD_SEGMENTS * SEGMENT) as f64 * (DATA_SEGMENTS * 188 * 8) as f64
}

/// A capture's transport stream: its programs, and the null packets' bitrate.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Multiplex {
    pub programs: Vec<Program>,
    pub null_bps: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Program {
    /// The program number, as a virtual channel names it.
    pub number: u16,
    pub pmt_pid: u16,
    pub pcr_pid: Option<u16>,
    /// Its streams' bitrates together.
    pub bitrate_bps: u32,
    pub streams: Vec<Stream>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stream {
    pub pid: u16,
    pub stream_type: u8,
    /// video, audio or data.
    pub kind: &'static str,
    /// The stream type's codec ("MPEG-2", "H.264", "AC-3", ...).
    pub codec: String,
    /// ISO 639, from the PMT.
    pub language: Option<String>,
    /// Its packets' share of the multiplex in the capture.
    pub bitrate_bps: u32,
    pub video: Option<VideoFormat>,
    pub audio: Option<AudioFormat>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VideoFormat {
    pub width: u16,
    pub height: u16,
    /// From the sequence extension, when there is one.
    pub progressive: Option<bool>,
    pub frame_rate: Option<f32>,
    /// The display aspect: "16:9", "4:3", "2.21:1", or "1:1" (square pixels).
    pub aspect: Option<&'static str>,
    /// Profile and level, such as "Main@High".
    pub profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioFormat {
    /// "5.1", "2.0", "1.0", or "1+1" for dual mono.
    pub channels: String,
    pub sample_rate_hz: u32,
    /// The bitrate its frames state.
    pub bitrate_bps: u32,
}

/// The programs in `packets` (those Reed-Solomon passed), their bitrates against `slots`: every
/// packet decoded, passed or not, each a 188-byte slot of the multiplex.
pub fn analyse(packets: &[&[u8; 188]], slots: usize) -> Multiplex {
    let mut count: HashMap<u16, usize> = HashMap::new();
    for p in packets {
        *count.entry(pid(p)).or_default() += 1;
    }
    let rate = |pid: u16| match slots {
        0 => 0,
        n => (count.get(&pid).copied().unwrap_or(0) as f64 / n as f64 * ts_rate_bps()).round() as u32,
    };
    let mut maps: Vec<(u16, u16)> = Vec::new();
    let mut sections = Sections::default();
    for p in packets {
        for (_, s) in sections.push(p, &[PAT_PID]) {
            for m in pat(&s) {
                if !maps.contains(&m) {
                    maps.push(m);
                }
            }
        }
    }
    let pmt_pids: Vec<u16> = maps.iter().map(|m| m.1).collect();
    let mut found: Vec<Program> = Vec::new();
    let mut sections = Sections::default();
    for p in packets {
        for (pid, s) in sections.push(p, &pmt_pids) {
            if let Some(prog) = pmt(pid, &s) {
                if !found.iter().any(|f| f.number == prog.number) {
                    found.push(prog);
                }
            }
        }
    }
    let wanted: Vec<u16> = found.iter().flat_map(|p| &p.streams).filter(|s| s.kind != "data").map(|s| s.pid).collect();
    let es = payloads(packets, &wanted);
    found.sort_by_key(|p| p.number);
    for prog in &mut found {
        for st in &mut prog.streams {
            st.bitrate_bps = rate(st.pid);
            let data = es.get(&st.pid).map_or(&[][..], Vec::as_slice);
            match st.stream_type {
                0x02 => st.video = mpeg2_video(data),
                0x81 => st.audio = ac3(data),
                _ => {}
            }
        }
        prog.bitrate_bps = prog.streams.iter().map(|s| s.bitrate_bps).sum();
    }
    Multiplex { programs: found, null_bps: rate(NULL_PID) }
}

/// A PAT's (program number, PMT PID) pairs; program 0 (the network PID) left out.
fn pat(s: &[u8]) -> Vec<(u16, u16)> {
    if s.len() < 12 || s[0] != PAT {
        return Vec::new();
    }
    s[8..s.len() - 4]
        .chunks_exact(4)
        .map(|e| (u16::from_be_bytes([e[0], e[1]]), u16::from_be_bytes([e[2] & 0x1F, e[3]])))
        .filter(|&(n, _)| n != 0)
        .collect()
}

/// A PMT: its program, PCR PID and streams (bitrates and formats to come).
fn pmt(pmt_pid: u16, s: &[u8]) -> Option<Program> {
    if s.len() < 16 || s[0] != PMT {
        return None;
    }
    let end = s.len() - 4;
    let pcr = u16::from_be_bytes([s[8] & 0x1F, s[9]]);
    let mut i = 12 + ((usize::from(s[10] & 0x0F) << 8) | usize::from(s[11]));
    let mut streams = Vec::new();
    while i + 5 <= end {
        let stream_type = s[i];
        let pid = u16::from_be_bytes([s[i + 1] & 0x1F, s[i + 2]]);
        let len = (usize::from(s[i + 3] & 0x0F) << 8) | usize::from(s[i + 4]);
        let desc = s.get(i + 5..(i + 5 + len).min(end)).unwrap_or(&[]);
        let (kind, codec) = codec(stream_type);
        streams.push(Stream {
            pid,
            stream_type,
            kind,
            codec,
            language: language(desc),
            bitrate_bps: 0,
            video: None,
            audio: None,
        });
        i += 5 + len;
    }
    Some(Program { number: u16::from_be_bytes([s[3], s[4]]), pmt_pid, pcr_pid: (pcr != 0x1FFF).then_some(pcr), bitrate_bps: 0, streams })
}

/// A stream type's kind and codec (ISO 13818-1, ATSC A/53 and A/72, SCTE).
fn codec(t: u8) -> (&'static str, String) {
    let (kind, name) = match t {
        0x01 => ("video", "MPEG-1"),
        0x02 => ("video", "MPEG-2"),
        0x1B => ("video", "H.264"),
        0x24 => ("video", "HEVC"),
        0x03 => ("audio", "MPEG-1 audio"),
        0x04 => ("audio", "MPEG-2 audio"),
        0x0F => ("audio", "AAC"),
        0x11 => ("audio", "AAC (LATM)"),
        0x81 => ("audio", "AC-3"),
        0x87 => ("audio", "E-AC-3"),
        0x05 => ("data", "private sections"),
        0x06 => ("data", "private data"),
        0x0B | 0x0D => ("data", "DSM-CC"),
        0x82 => ("data", "subtitles (SCTE 27)"),
        0x86 => ("data", "splice cues (SCTE 35)"),
        0x95 => ("data", "data service table (A/90)"),
        _ => return ("data", format!("type 0x{t:02X}")),
    };
    (kind, name.to_string())
}

/// The language of an ISO 639 descriptor among `d`.
fn language(mut d: &[u8]) -> Option<String> {
    while d.len() >= 2 {
        let (tag, len) = (d[0], d[1] as usize);
        let body = d.get(2..2 + len)?;
        if tag == ISO_639_LANGUAGE && len >= 3 && body[..3].iter().all(u8::is_ascii_alphabetic) {
            return Some(String::from_utf8_lossy(&body[..3]).to_lowercase());
        }
        d = &d[2 + len..];
    }
    None
}

/// Each PID's elementary stream in `wanted`: its packets' payloads, the PES headers taken out,
/// up to ES_KEEP bytes.
fn payloads(packets: &[&[u8; 188]], wanted: &[u16]) -> HashMap<u16, Vec<u8>> {
    let mut out: HashMap<u16, Vec<u8>> = HashMap::new();
    for p in packets {
        let id = pid(p);
        if !wanted.contains(&id) {
            continue;
        }
        let control = (p[3] >> 4) & 3;
        if control & 1 == 0 {
            continue;
        }
        let mut i = 4;
        if control & 2 != 0 {
            i += 1 + p[4] as usize;
        }
        let Some(mut payload) = p.get(i..) else { continue };
        let buf = out.entry(id).or_default();
        if p[1] & 0x40 != 0 {
            // A PES packet starts: its header is 9 bytes and the header data's length.
            if payload.len() < 9 || payload[..3] != [0, 0, 1] {
                continue;
            }
            payload = payload.get(9 + payload[8] as usize..).unwrap_or(&[]);
        } else if buf.is_empty() {
            // The stream's first bytes come from a PES packet's start.
            continue;
        }
        if buf.len() < ES_KEEP {
            buf.extend_from_slice(payload);
        }
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// MPEG-2 video's format: its first sequence header, and the sequence extension after it.
fn mpeg2_video(es: &[u8]) -> Option<VideoFormat> {
    let i = find(es, &[0, 0, 1, 0xB3])? + 4;
    let h = es.get(i..i + 4)?;
    let width = (u16::from(h[0]) << 4) | u16::from(h[1] >> 4);
    let height = (u16::from(h[1] & 0x0F) << 8) | u16::from(h[2]);
    let aspect = match h[3] >> 4 {
        1 => Some("1:1"),
        2 => Some("4:3"),
        3 => Some("16:9"),
        4 => Some("2.21:1"),
        _ => None,
    };
    let frame_rate = match h[3] & 0x0F {
        1 => Some(24000.0 / 1001.0),
        2 => Some(24.0),
        3 => Some(25.0),
        4 => Some(30000.0 / 1001.0),
        5 => Some(30.0),
        6 => Some(50.0),
        7 => Some(60000.0 / 1001.0),
        8 => Some(60.0),
        _ => None,
    };
    // The sequence extension (identifier 1) follows the header and its quantiser matrices.
    let ext = es
        .get(i..(i + 200).min(es.len()))
        .and_then(|after| find(after, &[0, 0, 1, 0xB5]).and_then(|j| after.get(j + 4..j + 6)))
        .filter(|e| e[0] >> 4 == 1);
    let (progressive, profile) = match ext {
        Some(e) => (Some(e[1] & 0x08 != 0), profile_level(((e[0] & 0x0F) << 4) | (e[1] >> 4))),
        None => (None, None),
    };
    (width > 0 && height > 0).then_some(VideoFormat { width, height, progressive, frame_rate, aspect, profile })
}

/// MPEG-2 video's profile and level indication, as "Main@High".
fn profile_level(pl: u8) -> Option<String> {
    let profile = match (pl >> 4) & 7 {
        1 => "High",
        2 => "Spatial",
        3 => "SNR",
        4 => "Main",
        5 => "Simple",
        _ => return None,
    };
    let level = match pl & 0x0F {
        4 => "High",
        6 => "High-1440",
        8 => "Main",
        10 => "Low",
        _ => return None,
    };
    (pl & 0x80 == 0).then(|| format!("{profile}@{level}"))
}

/// AC-3's nominal bitrates, kbit/s, by frmsizecod / 2.
const AC3_KBPS: [u32; 19] = [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640];

/// AC-3's format from a sync frame whose length lands on the next one's sync word.
fn ac3(es: &[u8]) -> Option<AudioFormat> {
    let mut from = 0;
    while let Some(k) = find(&es[from..], &[0x0B, 0x77]) {
        let i = from + k;
        from = i + 1;
        let Some(h) = es.get(i + 4..i + 8) else { break };
        let (fscod, frmsizecod, bsid) = (h[0] >> 6, (h[0] & 0x3F) as usize, h[1] >> 3);
        if fscod == 3 || frmsizecod >= 38 || bsid > 8 {
            continue;
        }
        let kbps = AC3_KBPS[frmsizecod / 2];
        let fs = [48_000u32, 44_100, 32_000][fscod as usize];
        // The frame's 16-bit words; 44.1 kHz frames of odd codes carry one more.
        let words = (kbps * 1000 * 1536 / (fs * 16)) as usize + usize::from(fscod == 1 && frmsizecod % 2 == 1);
        if es.get(i + 2 * words..i + 2 * words + 2) != Some(&[0x0B, 0x77]) {
            continue;
        }
        // acmod, then the mix levels it brings (2 bits each), then lfeon.
        let acmod = h[2] >> 5;
        let mut bit = 3;
        if acmod & 1 != 0 && acmod != 1 {
            bit += 2;
        }
        if acmod & 4 != 0 {
            bit += 2;
        }
        if acmod == 2 {
            bit += 2;
        }
        let lfe = (u16::from_be_bytes([h[2], h[3]]) >> (15 - bit)) & 1;
        let full = [2, 1, 2, 3, 3, 4, 4, 5][acmod as usize];
        let channels = if acmod == 0 { "1+1".to_string() } else { format!("{full}.{lfe}") };
        return Some(AudioFormat { channels, sample_rate_hz: fs, bitrate_bps: kbps * 1000 });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::atsc::ts::tests::{packets, section};

    /// `es` in one PES packet cut into TS packets on `pid`.
    fn pes(pid: u16, stream_id: u8, es: &[u8]) -> Vec<[u8; 188]> {
        let mut data = vec![0, 0, 1, stream_id, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
        data.extend_from_slice(es);
        data.chunks(184)
            .enumerate()
            .map(|(k, c)| {
                let mut p = [0xFFu8; 188];
                p[0] = 0x47;
                p[1] = (if k == 0 { 0x40 } else { 0 }) | (pid >> 8) as u8;
                p[2] = pid as u8;
                if c.len() == 184 {
                    p[3] = 0x10;
                    p[4..].copy_from_slice(c);
                } else {
                    // Short: an adaptation field of stuffing fills the packet.
                    p[3] = 0x30;
                    p[4] = (183 - c.len()) as u8;
                    if p[4] > 0 {
                        p[5] = 0;
                    }
                    p[188 - c.len()..].copy_from_slice(c);
                }
                p
            })
            .collect()
    }

    #[test]
    fn a_program_shows_its_streams_formats_and_bitrates() {
        // PAT: program 3 on PMT PID 0x30. PMT: PCR on 0x31; MPEG-2 video on 0x31, AC-3 in
        // English on 0x34.
        let pat = section(PAT, &[0x02, 0x5D, 0xC1, 0, 0, 0, 3, 0xE0, 0x30]);
        let pmt = section(PMT, &[0, 3, 0xC1, 0, 0, 0xE0, 0x31, 0xF0, 0, 0x02, 0xE0, 0x31, 0xF0, 0, 0x81, 0xE0, 0x34, 0xF0, 6, 0x0A, 4, b'e', b'n', b'g', 0]);
        // 1920 x 1080, 16:9, 29.97 frames a second; then Main@High, interlaced.
        let mut video = vec![0, 0, 1, 0xB3, 0x78, 0x04, 0x38, 0x34, 0xFF, 0xFF, 0xE0, 0x18, 0, 0, 1, 0xB5, 0x14, 0x42, 0x00, 0x01];
        video.resize(3000, 0x55);
        // Two AC-3 frames: 48 kHz, 384 kbit/s, 3/2 with LFE.
        let mut frame = vec![0x0B, 0x77, 0, 0, 0x1C, 0x40, 0xE1, 0x00];
        frame.resize(1536, 0);
        let audio = frame.repeat(2);
        let mut all: Vec<[u8; 188]> = packets(PAT_PID, &pat);
        all.extend(packets(0x30, &pmt));
        all.extend(pes(0x31, 0xE0, &video));
        all.extend(pes(0x34, 0xC0, &audio));
        let video_packets = all.iter().filter(|p| pid(p) == 0x31).count();
        let refs: Vec<&[u8; 188]> = all.iter().collect();
        let slots = 1000;
        let got = analyse(&refs, slots);
        assert_eq!((got.programs.len(), got.null_bps), (1, 0));
        let p = &got.programs[0];
        assert_eq!((p.number, p.pmt_pid, p.pcr_pid), (3, 0x30, Some(0x31)));
        assert_eq!(p.streams.len(), 2);
        let (v, a) = (&p.streams[0], &p.streams[1]);
        assert_eq!((v.kind, v.codec.as_str(), v.language.as_deref()), ("video", "MPEG-2", None));
        let f = v.video.as_ref().expect("the sequence header");
        assert_eq!((f.width, f.height, f.progressive, f.aspect, f.profile.as_deref()), (1920, 1080, Some(false), Some("16:9"), Some("Main@High")));
        assert!((f.frame_rate.unwrap_or(0.0) - 29.97).abs() < 0.01);
        let want = (video_packets as f64 / slots as f64 * ts_rate_bps()).round() as u32;
        assert_eq!(v.bitrate_bps, want);
        assert_eq!((a.kind, a.codec.as_str(), a.language.as_deref()), ("audio", "AC-3", Some("eng")));
        assert_eq!(a.audio, Some(AudioFormat { channels: "5.1".into(), sample_rate_hz: 48_000, bitrate_bps: 384_000 }));
        assert_eq!(p.bitrate_bps, v.bitrate_bps + a.bitrate_bps);
        assert!((ts_rate_bps() - 19_392_658.0).abs() < 1.0, "{}", ts_rate_bps());
    }

    #[test]
    fn a_false_ac3_sync_is_passed_over() {
        // 0x0B77 in the data before the real frames, with no frame after it.
        let mut es = vec![0x0B, 0x77, 0, 0, 0x1C, 0x40, 0xE1, 0, 1, 2, 3];
        // 48 kHz, 256 kbit/s (1,024 bytes), 2/0 without LFE.
        let mut frame = vec![0x0B, 0x77, 0, 0, 0x18, 0x40, 0x40, 0x00];
        frame.resize(1024, 0);
        es.extend(frame.repeat(2));
        let a = ac3(&es).expect("the real frames");
        assert_eq!((a.channels.as_str(), a.bitrate_bps), ("2.0", 256_000));
    }
}
