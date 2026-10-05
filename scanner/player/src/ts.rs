//! The MPEG-2 transport stream (ITU-T H.222.0): 188-byte packets in, the programme tables (PAT
//! 2.4.4.3, PMT 2.4.4.8) and each elementary stream's PES packets (2.4.3.6) out, with their
//! timestamps.
//!
//! A packet whose transport error indicator is set (the receiver's Reed-Solomon decoder could not
//! correct it) is dropped whole, since its PID cannot be trusted; the gap it leaves in its stream's
//! continuity counter marks the PES packet it belonged to as damaged.

use std::collections::{BTreeMap, HashMap, VecDeque};

pub const PACKET: usize = 188;
const SYNC: u8 = 0x47;

/// An elementary stream of a programme, from its PMT.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    pub pid: u16,
    /// Table 2-34; ATSC A/53 part 3 adds 0x81, AC-3 audio.
    pub stream_type: u8,
}

pub const STREAM_MPEG2_VIDEO: u8 = 0x02;
pub const STREAM_AC3: u8 = 0x81;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub number: u16,
    pub pmt_pid: u16,
    pub pcr_pid: u16,
    pub streams: Vec<Stream>,
}

/// One PES packet's payload.
#[derive(Clone, Debug)]
pub struct Pes {
    pub pid: u16,
    /// Presentation and decoding timestamps, 90 kHz, 33 bits.
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    pub data: Vec<u8>,
    /// Packets of it were lost or it was cut short.
    pub damaged: bool,
}

/// A section being gathered from a PID's packets.
#[derive(Default)]
struct Section {
    data: Vec<u8>,
    /// Packets were lost since the section began.
    broken: bool,
}

/// A PES packet being gathered.
struct PesBuf {
    data: Vec<u8>,
    damaged: bool,
}

/// CRC-32 of MPEG-2 sections (Annex A: x^32 + x^26 + ... + 1, initial all ones, no reflection);
/// over a whole section, its CRC included, it is zero.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

/// A 33-bit timestamp from its five bytes (2.4.3.7: 4 bits, then 3, 15 and 15 between markers).
fn timestamp(b: &[u8]) -> u64 {
    (u64::from(b[0] >> 1) & 7) << 30 | u64::from(b[1]) << 22 | u64::from(b[2] >> 1) << 15 | u64::from(b[3]) << 7 | u64::from(b[4] >> 1)
}

/// The programme clock reference a packet carries, in 27 MHz ticks (2.4.3.5: base × 300 + extension).
pub fn packet_pcr(p: &[u8]) -> Option<u64> {
    if p.len() < 12 || p[0] != SYNC || p[3] & 0x20 == 0 || p[4] < 7 || p[5] & 0x10 == 0 {
        return None;
    }
    let base = u64::from(p[6]) << 25 | u64::from(p[7]) << 17 | u64::from(p[8]) << 9 | u64::from(p[9]) << 1 | u64::from(p[10] >> 7);
    let ext = u64::from(p[10] & 1) << 8 | u64::from(p[11]);
    Some(base * 300 + ext)
}

pub fn packet_pid(p: &[u8]) -> u16 {
    u16::from(p[1] & 0x1F) << 8 | u16::from(p[2])
}

pub struct Demux {
    /// Bytes not yet a whole packet.
    pending: Vec<u8>,
    /// Each PID's last continuity counter (none: not seen, or after a discontinuity).
    cc: Vec<Option<u8>>,
    /// Programme number to PMT PID, from the PAT.
    pat: BTreeMap<u16, u16>,
    programs: BTreeMap<u16, Program>,
    sections: HashMap<u16, Section>,
    pes: HashMap<u16, PesBuf>,
    /// Only this programme's streams, or every programme's.
    only: Option<u16>,
    out: VecDeque<Pes>,
}

impl Default for Demux {
    fn default() -> Demux {
        Demux {
            pending: Vec::new(),
            cc: vec![None; 8192],
            pat: BTreeMap::new(),
            programs: BTreeMap::new(),
            sections: HashMap::new(),
            pes: HashMap::new(),
            only: None,
            out: VecDeque::new(),
        }
    }
}

impl Demux {
    /// Follows one programme's streams (None: every programme's).
    pub fn follow(&mut self, program: Option<u16>) {
        self.only = program;
        self.pes.retain(|&pid, _| Self::wanted(&self.programs, program, pid));
    }

    /// The programmes their PMTs have described so far.
    pub fn programs(&self) -> impl Iterator<Item = &Program> {
        self.programs.values()
    }

    /// The next complete PES packet.
    pub fn next_pes(&mut self) -> Option<Pes> {
        self.out.pop_front()
    }

    /// Any run of the stream's bytes; packets are found by their sync bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let mut at = 0;
        while self.pending.len() - at >= PACKET {
            if self.pending[at] != SYNC {
                at += 1;
                continue;
            }
            // Out of step: a sync byte one packet on too, if the buffer reaches it.
            if self.pending.len() - at > PACKET && self.pending[at + PACKET] != SYNC {
                at += 1;
                continue;
            }
            let p: [u8; PACKET] = self.pending[at..at + PACKET].try_into().unwrap();
            self.packet(&p);
            at += PACKET;
        }
        self.pending.drain(..at);
    }

    /// The end of the stream: PES packets still open are complete as far as they go.
    pub fn finish(&mut self) {
        let mut pids: Vec<u16> = self.pes.keys().copied().collect();
        pids.sort();
        for pid in pids {
            self.flush(pid, false);
        }
    }

    fn wanted(programs: &BTreeMap<u16, Program>, only: Option<u16>, pid: u16) -> bool {
        programs.values().filter(|p| !matches!(only, Some(n) if n != p.number)).any(|p| p.streams.iter().any(|s| s.pid == pid))
    }

    pub fn packet(&mut self, p: &[u8; PACKET]) {
        if p[1] & 0x80 != 0 {
            return;
        }
        let start = p[1] & 0x40 != 0;
        let pid = packet_pid(p);
        let control = (p[3] >> 4) & 3;
        let cc = p[3] & 15;
        let mut at = 4;
        let mut discontinuity = false;
        if control & 2 != 0 {
            let len = usize::from(p[4]);
            if len > 183 {
                return;
            }
            discontinuity = len > 0 && p[5] & 0x80 != 0;
            at = 5 + len;
        }
        if control & 1 == 0 || pid == 0x1FFF {
            return;
        }
        // Continuity (2.4.3.3): one more than the last, or the last again for a duplicate packet.
        let last = self.cc[usize::from(pid)].replace(cc);
        let lost = match last {
            Some(l) if !discontinuity => {
                if cc == l {
                    return;
                }
                cc != (l + 1) & 15
            }
            _ => false,
        };
        let payload = &p[at..];
        if pid == 0 || self.pat.values().any(|&m| m == pid) {
            self.psi(pid, start, lost, payload);
        } else if Self::wanted(&self.programs, self.only, pid) {
            self.pes_payload(pid, start, lost, payload);
        }
    }

    fn psi(&mut self, pid: u16, start: bool, lost: bool, payload: &[u8]) {
        let mut done = Vec::new();
        let sec = self.sections.entry(pid).or_default();
        if lost {
            sec.broken = true;
        }
        let mut rest = payload;
        if start {
            let pointer = usize::from(rest[0]);
            if 1 + pointer > rest.len() {
                sec.data.clear();
                return;
            }
            // The previous section's tail, then new sections.
            if !sec.broken && !sec.data.is_empty() {
                sec.data.extend_from_slice(&rest[1..1 + pointer]);
                done.push(std::mem::take(&mut sec.data));
            }
            sec.data.clear();
            sec.broken = false;
            rest = &rest[1 + pointer..];
            sec.data.extend_from_slice(rest);
        } else if !sec.broken && !sec.data.is_empty() {
            sec.data.extend_from_slice(rest);
        }
        // Every whole section gathered so far; stuffing (0xFF) ends the packet's sections.
        loop {
            let d = &sec.data;
            if d.len() < 3 || d[0] == 0xFF {
                break;
            }
            let len = 3 + (usize::from(d[1] & 0x0F) << 8 | usize::from(d[2]));
            if d.len() < len {
                break;
            }
            done.push(d[..len].to_vec());
            sec.data.drain(..len);
        }
        if sec.data.first() == Some(&0xFF) {
            sec.data.clear();
        }
        for s in done {
            self.section(pid, &s);
        }
    }

    fn section(&mut self, pid: u16, s: &[u8]) {
        let len = s.len();
        if len < 3 {
            return;
        }
        let declared = 3 + (usize::from(s[1] & 0x0F) << 8 | usize::from(s[2]));
        // Long-form sections only (PAT and PMT are), current, whole and their CRC good.
        if declared != len || len < 12 || s[1] & 0x80 == 0 || s[5] & 1 == 0 || crc32(s) != 0 {
            return;
        }
        let body = &s[8..len - 4];
        let id_ext = u16::from(s[3]) << 8 | u16::from(s[4]);
        match (pid, s[0]) {
            (0, 0x00) => {
                self.pat = body
                    .chunks_exact(4)
                    .map(|e| (u16::from(e[0]) << 8 | u16::from(e[1]), u16::from(e[2] & 0x1F) << 8 | u16::from(e[3])))
                    .filter(|&(number, _)| number != 0)
                    .collect();
                self.programs.retain(|n, _| self.pat.contains_key(n));
            }
            (_, 0x02) if self.pat.get(&id_ext) == Some(&pid) => {
                if body.len() < 4 {
                    return;
                }
                let pcr_pid = u16::from(body[0] & 0x1F) << 8 | u16::from(body[1]);
                let info = usize::from(body[2] & 0x0F) << 8 | usize::from(body[3]);
                let mut at = 4 + info;
                let mut streams = Vec::new();
                while at + 5 <= body.len() {
                    let e = &body[at..];
                    streams.push(Stream { stream_type: e[0], pid: u16::from(e[1] & 0x1F) << 8 | u16::from(e[2]) });
                    at += 5 + (usize::from(e[3] & 0x0F) << 8 | usize::from(e[4]));
                }
                self.programs.insert(id_ext, Program { number: id_ext, pmt_pid: pid, pcr_pid, streams });
            }
            _ => {}
        }
    }

    fn pes_payload(&mut self, pid: u16, start: bool, lost: bool, payload: &[u8]) {
        if start {
            self.flush(pid, lost);
            self.pes.insert(pid, PesBuf { data: payload.to_vec(), damaged: false });
        } else if let Some(buf) = self.pes.get_mut(&pid) {
            buf.damaged |= lost;
            buf.data.extend_from_slice(payload);
        } else {
            return;
        }
        // A PES packet of declared length is complete at that length.
        let buf = &self.pes[&pid];
        if buf.data.len() >= 6 {
            let declared = usize::from(buf.data[4]) << 8 | usize::from(buf.data[5]);
            if declared > 0 && buf.data.len() >= 6 + declared {
                self.flush(pid, false);
            }
        }
    }

    /// Emits the PID's open PES packet; `lost`: packets were lost at its end.
    fn flush(&mut self, pid: u16, lost: bool) {
        let Some(buf) = self.pes.remove(&pid) else { return };
        let d = &buf.data;
        if d.len() < 9 || d[0..3] != [0, 0, 1] {
            return;
        }
        let declared = usize::from(d[4]) << 8 | usize::from(d[5]);
        let end = if declared > 0 { (6 + declared).min(d.len()) } else { d.len() };
        let short = declared > 0 && d.len() < 6 + declared;
        // Streams without the optional header (2.4.3.7): padding, private 2, ECM, EMM, directory,
        // DSMCC, H.222.1 type E.
        let (pts, dts, body) = if matches!(d[3], 0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xF2 | 0xF8 | 0xFF) {
            (None, None, 6)
        } else {
            let flags = d[7] >> 6;
            let body = 9 + usize::from(d[8]);
            if body > end || (flags & 2 != 0 && d.len() < 14) || (flags == 3 && d.len() < 19) {
                return;
            }
            let pts = (flags & 2 != 0).then(|| timestamp(&d[9..14]));
            let dts = if flags == 3 { Some(timestamp(&d[14..19])) } else { pts };
            (pts, dts, body)
        };
        self.out.push_back(Pes { pid, pts, dts, data: d[body..end].to_vec(), damaged: buf.damaged || lost || short });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A section with its CRC appended.
    fn with_crc(mut s: Vec<u8>) -> Vec<u8> {
        let c = crc32(&s);
        s.extend_from_slice(&c.to_be_bytes());
        s
    }

    fn packet(pid: u16, start: bool, cc: u8, payload: &[u8]) -> [u8; PACKET] {
        let mut p = [0xFFu8; PACKET];
        p[0] = SYNC;
        p[1] = u8::from(start) << 6 | (pid >> 8) as u8;
        p[2] = pid as u8;
        let stuffing = PACKET - 4 - payload.len();
        if stuffing == 0 {
            p[3] = 0x10 | cc;
            p[4..].copy_from_slice(payload);
        } else {
            // An adaptation field of stuffing before the payload.
            p[3] = 0x30 | cc;
            p[4] = (stuffing - 1) as u8;
            if stuffing > 1 {
                p[5] = 0;
            }
            p[4 + stuffing..].copy_from_slice(payload);
        }
        p
    }

    fn tables(d: &mut Demux) {
        let pat = with_crc(vec![0x00, 0xB0, 13, 0, 1, 0xC1, 0, 0, 0, 3, 0xE0, 0x30]);
        let mut pmt = vec![0x02, 0xB0, 0, 0, 3, 0xC1, 0, 0, 0xE0, 0x31, 0xF0, 0];
        pmt.extend_from_slice(&[0x02, 0xE0, 0x31, 0xF0, 0, 0x81, 0xE0, 0x34, 0xF0, 0]);
        pmt[2] = (pmt.len() + 4 - 3) as u8;
        let pmt = with_crc(pmt);
        d.packet(&packet(0, true, 0, &[[0u8].as_slice(), &pat].concat()));
        d.packet(&packet(0x30, true, 0, &[[0u8].as_slice(), &pmt].concat()));
    }

    #[test]
    fn tables_then_a_pes_packet_with_its_timestamps() {
        let mut d = Demux::default();
        tables(&mut d);
        let progs: Vec<&Program> = d.programs().collect();
        assert_eq!(progs.len(), 1);
        assert_eq!(progs[0].pcr_pid, 0x31);
        assert_eq!(progs[0].streams, vec![Stream { pid: 0x31, stream_type: 2 }, Stream { pid: 0x34, stream_type: 0x81 }]);
        // PTS 0x1_2345_6789 and DTS 0x0_8765_4321 over two packets, video-style (length 0).
        let (pts, dts) = (0x1_2345_6789u64, 0x0_8765_4321u64);
        let ts = |prefix: u8, t: u64| [prefix << 4 | ((t >> 29) & 0xE) as u8 | 1, (t >> 22) as u8, ((t >> 14) & 0xFE) as u8 | 1, (t >> 7) as u8, ((t << 1) & 0xFE) as u8 | 1];
        let mut pes = vec![0, 0, 1, 0xE0, 0, 0, 0x80, 0xC0, 10];
        pes.extend_from_slice(&ts(3, pts));
        pes.extend_from_slice(&ts(1, dts));
        let body: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        pes.extend_from_slice(&body);
        d.packet(&packet(0x31, true, 0, &pes[..184]));
        d.packet(&packet(0x31, false, 1, &pes[184..]));
        assert!(d.next_pes().is_none());
        d.finish();
        let got = d.next_pes().unwrap();
        assert_eq!((got.pid, got.pts, got.dts, got.damaged), (0x31, Some(pts), Some(dts), false));
        assert_eq!(got.data, body);
    }

    #[test]
    fn a_lost_packet_damages_its_pes_packet_and_errored_packets_are_dropped() {
        let mut d = Demux::default();
        tables(&mut d);
        let mut pes = vec![0, 0, 1, 0xBD, 0, 0, 0x80, 0x80, 5, 0x21, 0, 1, 0, 1];
        pes.extend((0..400u32).map(|i| i as u8));
        let n = pes.len() - 6;
        pes[4] = (n >> 8) as u8;
        pes[5] = n as u8;
        let chunks: Vec<&[u8]> = pes.chunks(184).collect();
        d.packet(&packet(0x34, true, 5, chunks[0]));
        // The second packet arrives with its error indicator set: dropped, leaving a gap.
        let mut bad = packet(0x34, false, 6, chunks[1]);
        bad[1] |= 0x80;
        d.packet(&bad);
        d.packet(&packet(0x34, false, 7, chunks[2]));
        d.finish();
        let got = d.next_pes().unwrap();
        assert!(got.damaged);
        assert_eq!(got.pts, Some(0));
    }

    #[test]
    fn the_crc_of_a_section_with_its_crc_is_zero() {
        assert_eq!(crc32(&with_crc(b"transport".to_vec())), 0);
    }
}
