//! What a station says about itself (ATSC A/65 PSIP, and the MPEG-2 PAT): its transport stream
//! id, its virtual channels (the major.minor numbers viewers see, each with its short name, its
//! long name when sent, its program number and service type) and its clock.

use serde::Serialize;

use super::ts::{PAT_PID, PSIP_PID};

const PAT: u8 = 0x00;
const TVCT: u8 = 0xC8;
const CVCT: u8 = 0xC9;
const STT: u8 = 0xCD;
const EXTENDED_CHANNEL_NAME: u8 = 0xA0;
/// GPS time's epoch (1980-01-06) in Unix seconds.
const GPS_EPOCH_UNIX: i64 = 315_964_800;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VirtualChannel {
    pub major: u16,
    pub minor: u16,
    pub short_name: String,
    /// From the extended channel name descriptor, when sent.
    pub long_name: Option<String>,
    pub program: u16,
    /// 2: digital television, 3: audio, 4: data.
    pub service_type: u8,
    pub source_id: u16,
    pub hidden: bool,
    /// Scrambled.
    pub access_controlled: bool,
}

/// What a channel's PSIP said, as far as it was heard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Psip {
    pub tsid: Option<u16>,
    pub channels: Vec<VirtualChannel>,
    /// The station's clock (STT), Unix seconds.
    pub time_unix: Option<i64>,
}

impl Psip {
    pub fn pids() -> [u16; 2] {
        [PAT_PID, PSIP_PID]
    }

    /// Take in one CRC-checked section.
    pub fn section(&mut self, pid: u16, s: &[u8]) {
        if s.len() < 12 {
            return;
        }
        match (pid, s[0]) {
            (PAT_PID, PAT) => self.tsid = Some(u16::from_be_bytes([s[3], s[4]])),
            (PSIP_PID, TVCT | CVCT) => self.vct(s),
            (PSIP_PID, STT) if s.len() >= 17 => {
                let gps = u32::from_be_bytes([s[9], s[10], s[11], s[12]]);
                self.time_unix = Some(GPS_EPOCH_UNIX + i64::from(gps) - i64::from(s[13]));
            }
            _ => {}
        }
    }

    fn vct(&mut self, s: &[u8]) {
        if self.tsid.is_none() {
            self.tsid = Some(u16::from_be_bytes([s[3], s[4]]));
        }
        let end = s.len() - 4;
        let n = s[9] as usize;
        let mut i = 10;
        for _ in 0..n {
            if i + 32 > end {
                return;
            }
            let c = &s[i..];
            let short_name = utf16(&c[..14]);
            let major = (u16::from(c[14] & 0x0F) << 6) | u16::from(c[15] >> 2);
            let minor = (u16::from(c[15] & 0x03) << 8) | u16::from(c[16]);
            let descriptors = (usize::from(c[30] & 0x03) << 8) | usize::from(c[31]);
            let desc = c.get(32..32 + descriptors).unwrap_or(&[]);
            let channel = VirtualChannel {
                major,
                minor,
                short_name,
                long_name: long_name(desc),
                program: u16::from_be_bytes([c[24], c[25]]),
                service_type: c[27] & 0x3F,
                source_id: u16::from_be_bytes([c[28], c[29]]),
                hidden: c[26] & 0x10 != 0,
                access_controlled: c[26] & 0x20 != 0,
            };
            match self.channels.iter_mut().find(|x| x.major == major && x.minor == minor) {
                Some(x) => *x = channel,
                None => self.channels.push(channel),
            }
            i += 32 + descriptors;
        }
        self.channels.sort_by_key(|c| (c.major, c.minor));
    }
}

/// A short name: up to 7 UTF-16 characters, padded with NULs.
fn utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|p| u16::from_be_bytes([p[0], p[1]])).take_while(|&u| u != 0).collect();
    String::from_utf16_lossy(&units).trim().to_string()
}

/// The extended channel name descriptor's first string, when it is uncompressed Latin-1.
fn long_name(mut d: &[u8]) -> Option<String> {
    while d.len() >= 2 {
        let (tag, len) = (d[0], d[1] as usize);
        let body = d.get(2..2 + len)?;
        if tag == EXTENDED_CHANNEL_NAME {
            return multiple_string(body);
        }
        d = &d[2 + len..];
    }
    None
}

/// The first segment of a multiple string structure (A/65 6.10), uncompressed, mode 0 (Latin-1).
fn multiple_string(b: &[u8]) -> Option<String> {
    // number_strings, then ISO 639 language (3), number_segments, then compression, mode, length.
    if b.len() < 8 || b[0] == 0 || b[4] == 0 || b[5] != 0 || b[6] != 0 {
        return None;
    }
    let text = b.get(8..8 + b[7] as usize)?;
    let s: String = text.iter().map(|&c| c as char).collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::atsc::ts::tests::{packets, section};
    use crate::protocol::atsc::ts::Sections;

    fn channel(name: &str, major: u16, minor: u16, program: u16, long: Option<&str>) -> Vec<u8> {
        let mut c = Vec::new();
        let mut units: Vec<u16> = name.encode_utf16().collect();
        units.resize(7, 0);
        for u in units {
            c.extend_from_slice(&u.to_be_bytes());
        }
        c.push(0xF0 | (major >> 6) as u8);
        c.push(((major & 0x3F) << 2) as u8 | (minor >> 8) as u8);
        c.push(minor as u8);
        c.push(0x04); // 8-VSB
        c.extend_from_slice(&[0, 0, 0, 0]);
        c.extend_from_slice(&601u16.to_be_bytes());
        c.extend_from_slice(&program.to_be_bytes());
        c.push(0x0D);
        c.push(0xC2); // digital television
        c.extend_from_slice(&(program + 100).to_be_bytes());
        let desc = long.map(|l| {
            let mut m = vec![1, b'e', b'n', b'g', 1, 0, 0, l.len() as u8];
            m.extend_from_slice(l.as_bytes());
            let mut d = vec![EXTENDED_CHANNEL_NAME, m.len() as u8];
            d.extend(m);
            d
        });
        let dlen = desc.as_ref().map_or(0, Vec::len);
        c.push(0xFC | (dlen >> 8) as u8);
        c.push(dlen as u8);
        c.extend(desc.unwrap_or_default());
        c
    }

    fn tvct(tsid: u16, chans: &[Vec<u8>]) -> Vec<u8> {
        let mut body = tsid.to_be_bytes().to_vec();
        body.extend_from_slice(&[0xC1, 0, 0, 0, chans.len() as u8]);
        for c in chans {
            body.extend_from_slice(c);
        }
        body.extend_from_slice(&[0xFC, 0]);
        section(TVCT, &body)
    }

    #[test]
    fn a_vct_names_the_virtual_channels() {
        let sec = tvct(601, &[channel("WJXT-HD", 4, 1, 7, Some("News4JAX")), channel("WCWJ-HD", 17, 1, 3, None)]);
        let mut p = Psip::default();
        let mut s = Sections::default();
        for pk in packets(PSIP_PID, &sec) {
            for (pid, sec) in s.push(&pk, &Psip::pids()) {
                p.section(pid, &sec);
            }
        }
        assert_eq!(p.tsid, Some(601));
        let names: Vec<_> = p.channels.iter().map(|c| (c.major, c.minor, c.short_name.as_str(), c.program, c.long_name.as_deref())).collect();
        assert_eq!(names, [(4, 1, "WJXT-HD", 7, Some("News4JAX")), (17, 1, "WCWJ-HD", 3, None)]);
        assert_eq!(p.channels[0].service_type, 2);
    }

    #[test]
    fn the_stt_gives_the_station_clock_in_utc() {
        // 2026-10-04T02:06:50Z is GPS 1475201228 with 18 leap seconds.
        let gps: u32 = (1_791_079_610 - GPS_EPOCH_UNIX + 18) as u32;
        let mut body = vec![0, 0, 0xC1, 0, 0, 0];
        body.extend_from_slice(&gps.to_be_bytes());
        body.extend_from_slice(&[18, 0x60, 0x00]);
        let mut p = Psip::default();
        p.section(PSIP_PID, &section(STT, &body));
        assert_eq!(p.time_unix, Some(1_791_079_610));
    }
}
