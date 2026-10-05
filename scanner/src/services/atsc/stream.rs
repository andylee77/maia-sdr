//! The TV stream: an RF channel's transport stream served to players as it plays (VLC and other
//! players over HTTP, and the Viewer's). Its source is the channel's recording on the SD card
//! (`tv/rf<N>.ts`), played at the pace its programme clock references set and from the start
//! again at its end; each player gets its own reading.
//!
//! With a programme asked for, only its packets pass: the PAT rewritten to name it alone, its
//! PMT, its elementary streams, its clock's PID and PSIP's base PID (the station's names).
//! Without one, the whole multiplex.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::protocol::atsc::ts::{crc32, pid, Sections, PAT_PID, PSIP_PID};

pub const PACKET: usize = 188;
const SYNC: u8 = 0x47;
/// The programme clock reference's rate.
const PCR_HZ: f64 = 27e6;
/// A clock reference this far from the last, either way, starts the pacing again (the
/// recording's start after its end).
const PCR_JUMP: u64 = 27_000_000;

/// A recording a player can be served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TvRecording {
    pub rf: u8,
    pub bytes: u64,
    /// The programme numbers its first PAT lists.
    pub programs: Vec<u16>,
}

pub fn recording_path(dir: &Path, rf: u8) -> PathBuf {
    dir.join(format!("rf{rf}.ts"))
}

/// The recordings in `dir`, by RF channel.
pub fn recordings(dir: &Path) -> Vec<TvRecording> {
    let mut out: Vec<TvRecording> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let rf = name.strip_prefix("rf")?.strip_suffix(".ts")?.parse().ok()?;
            Some(TvRecording { rf, bytes: e.metadata().ok()?.len(), programs: programs(&e.path()) })
        })
        .collect();
    out.sort_by_key(|r| r.rf);
    out
}

/// The programme numbers in a recording's first PAT (found within its first 4 MB, about 1.7 s
/// of a multiplex; a PAT comes at least every 100 ms).
pub fn programs(path: &Path) -> Vec<u16> {
    use std::io::Read;
    let mut head = Vec::new();
    if std::fs::File::open(path).and_then(|f| f.take(4 << 20).read_to_end(&mut head)).is_err() {
        return Vec::new();
    }
    let mut sections = Sections::default();
    for chunk in head.chunks_exact(PACKET) {
        let p: &[u8; PACKET] = chunk.try_into().expect("a packet's length");
        if p[0] != SYNC {
            continue;
        }
        if let Some((_, pat)) = sections.push(p, &[PAT_PID]).into_iter().find(|(_, s)| s[0] == 0x00 && s.len() >= 12) {
            return pat[8..pat.len() - 4].chunks_exact(4).map(|e| u16::from_be_bytes([e[0], e[1]])).filter(|&n| n != 0).collect();
        }
    }
    Vec::new()
}

/// A packet's programme clock reference, in 27 MHz ticks (base × 300 + extension).
pub fn pcr(p: &[u8; PACKET]) -> Option<u64> {
    if p[3] & 0x20 == 0 || p[4] < 7 || p[5] & 0x10 == 0 {
        return None;
    }
    let base = (u64::from(p[6]) << 25) | (u64::from(p[7]) << 17) | (u64::from(p[8]) << 9) | (u64::from(p[9]) << 1) | u64::from(p[10] >> 7);
    let ext = (u64::from(p[10] & 1) << 8) | u64::from(p[11]);
    Some(base * 300 + ext)
}

/// One programme out of a multiplex (see the module header). Nothing passes until the PAT and
/// the programme's PMT have been read; then the rewritten PAT goes first.
pub struct ProgramFilter {
    program: u16,
    sections: Sections,
    pmt_pid: Option<u16>,
    /// The PMT's, the streams' and the clock's PIDs, once the PMT is read.
    pass: BTreeSet<u16>,
    pcr_pid: Option<u16>,
    /// The PAT naming the programme alone, as a section.
    pat: Option<Vec<u8>>,
    pat_cc: u8,
}

impl ProgramFilter {
    pub fn new(program: u16) -> ProgramFilter {
        ProgramFilter { program, sections: Sections::default(), pmt_pid: None, pass: BTreeSet::new(), pcr_pid: None, pat: None, pat_cc: 0 }
    }

    pub fn pcr_pid(&self) -> Option<u16> {
        self.pcr_pid
    }

    fn ready(&self) -> bool {
        self.pat.is_some() && !self.pass.is_empty()
    }

    /// Appends to `out` what of packet `p` passes.
    pub fn push(&mut self, p: &[u8; PACKET], out: &mut Vec<u8>) {
        let was_ready = self.ready();
        let watch: Vec<u16> = std::iter::once(PAT_PID).chain(self.pmt_pid).collect();
        for (from, sec) in self.sections.push(p, &watch) {
            if from == PAT_PID && sec[0] == 0x00 {
                self.read_pat(&sec);
            } else if Some(from) == self.pmt_pid && sec[0] == 0x02 {
                self.read_pmt(from, &sec);
            }
        }
        let id = pid(p);
        if !self.ready() {
            return;
        }
        // The PAT once the programme is known, then at the original's cadence.
        if !was_ready || (id == PAT_PID && p[1] & 0x40 != 0) {
            if let Some(pat) = &self.pat {
                out.extend_from_slice(&section_packet(PAT_PID, pat, self.pat_cc));
                self.pat_cc = (self.pat_cc + 1) & 15;
            }
        }
        if id != PAT_PID && (self.pass.contains(&id) || id == PSIP_PID) {
            out.extend_from_slice(p);
        }
    }

    fn read_pat(&mut self, sec: &[u8]) {
        // Current sections only (current_next_indicator).
        if sec.len() < 12 || sec[5] & 1 == 0 {
            return;
        }
        let entries = &sec[8..sec.len() - 4];
        let Some(pmt_pid) = entries
            .chunks_exact(4)
            .find(|e| u16::from_be_bytes([e[0], e[1]]) == self.program)
            .map(|e| (u16::from(e[2] & 0x1F) << 8) | u16::from(e[3]))
        else {
            return;
        };
        if self.pmt_pid != Some(pmt_pid) {
            self.pass.clear();
            self.pcr_pid = None;
        }
        self.pmt_pid = Some(pmt_pid);
        // The same transport stream id and version, one programme.
        let mut pat = vec![0x00, 0xB0, 13, sec[3], sec[4], sec[5], 0, 0];
        pat.extend_from_slice(&self.program.to_be_bytes());
        pat.extend_from_slice(&(0xE000 | pmt_pid).to_be_bytes());
        pat.extend_from_slice(&crc32(&pat).to_be_bytes());
        self.pat = Some(pat);
    }

    fn read_pmt(&mut self, from: u16, sec: &[u8]) {
        if sec.len() < 16 || sec[5] & 1 == 0 || u16::from_be_bytes([sec[3], sec[4]]) != self.program {
            return;
        }
        let body = &sec[8..sec.len() - 4];
        let pcr_pid = (u16::from(body[0] & 0x1F) << 8) | u16::from(body[1]);
        let mut at = 4 + ((usize::from(body[2] & 0x0F) << 8) | usize::from(body[3]));
        let mut pass = BTreeSet::from([from, pcr_pid]);
        while at + 5 <= body.len() {
            let e = &body[at..];
            pass.insert((u16::from(e[1] & 0x1F) << 8) | u16::from(e[2]));
            at += 5 + ((usize::from(e[3] & 0x0F) << 8) | usize::from(e[4]));
        }
        self.pass = pass;
        self.pcr_pid = Some(pcr_pid);
    }
}

/// A section that fits one packet, as that packet: the pointer field, the section, stuffing.
fn section_packet(pid: u16, section: &[u8], cc: u8) -> [u8; PACKET] {
    let mut p = [0xFFu8; PACKET];
    p[0] = SYNC;
    p[1] = 0x40 | (pid >> 8) as u8;
    p[2] = pid as u8;
    p[3] = 0x10 | (cc & 15);
    p[4] = 0;
    p[5..5 + section.len()].copy_from_slice(section);
    p
}

/// Plays the recording at `path` into `tx` until the receiver is gone: programme `program` (or
/// the whole multiplex), paced by its clock references, from the start again at its end. Each
/// piece sent is what came between two clock references, sent when the first of them is due.
pub async fn play(path: PathBuf, program: Option<u16>, tx: mpsc::Sender<Vec<u8>>) -> anyhow::Result<()> {
    let mut filter = program.map(ProgramFilter::new);
    // The whole multiplex is paced by the first PID found carrying a clock reference.
    let mut clock_pid: Option<u16> = None;
    // A clock reference and when it was due; the last one read.
    let mut anchor: Option<(u64, Instant)> = None;
    let mut last: Option<u64> = None;
    let mut out: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 256 * PACKET];
    loop {
        let mut file = tokio::fs::File::open(&path).await?;
        let mut have = 0;
        let mut paced = false;
        loop {
            let n = file.read(&mut buf[have..]).await?;
            if n == 0 {
                break;
            }
            have += n;
            let whole = have / PACKET * PACKET;
            for chunk in buf[..whole].chunks_exact(PACKET) {
                let mut p: [u8; PACKET] = chunk.try_into().expect("a packet's length");
                if p[0] != SYNC {
                    continue;
                }
                let id = pid(&p);
                if let Some(v) = pcr(&p) {
                    let by = match &filter {
                        Some(f) => f.pcr_pid(),
                        None => Some(*clock_pid.get_or_insert(id)),
                    };
                    if by == Some(id) {
                        paced = true;
                        match (anchor, last) {
                            (Some((v0, t0)), Some(l)) if v >= l && v - l < PCR_JUMP => {
                                let due = t0 + Duration::from_secs_f64((v - v0) as f64 / PCR_HZ);
                                if due > Instant::now() {
                                    if !out.is_empty() && tx.send(std::mem::take(&mut out)).await.is_err() {
                                        return Ok(());
                                    }
                                    tokio::time::sleep_until(due).await;
                                }
                            }
                            (a, _) => {
                                // Past a jump the clock is discontinuous (2.4.3.5): say so.
                                if a.is_some() {
                                    p[5] |= 0x80;
                                }
                                anchor = Some((v, Instant::now()));
                            }
                        }
                        last = Some(v);
                    }
                }
                match &mut filter {
                    Some(f) => f.push(&p, &mut out),
                    None => out.extend_from_slice(&p),
                }
            }
            buf.copy_within(whole..have, 0);
            have -= whole;
        }
        if !paced {
            anyhow::bail!("{}: no clock references to play it by", path.display());
        }
        if tx.is_closed() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::atsc::ts::tests::{packets, section};

    fn pat(entries: &[(u16, u16)]) -> Vec<u8> {
        let mut body = vec![0x12, 0x34, 0xC3, 0, 0];
        for &(program, pmt) in entries {
            body.extend_from_slice(&program.to_be_bytes());
            body.extend_from_slice(&(0xE000 | pmt).to_be_bytes());
        }
        section(0x00, &body)
    }

    fn pmt(program: u16, pcr: u16, streams: &[(u8, u16)]) -> Vec<u8> {
        let mut body = program.to_be_bytes().to_vec();
        body.extend_from_slice(&[0xC1, 0, 0]);
        body.extend_from_slice(&(0xE000 | pcr).to_be_bytes());
        body.extend_from_slice(&[0xF0, 0]);
        for &(kind, pid) in streams {
            body.push(kind);
            body.extend_from_slice(&(0xE000 | pid).to_be_bytes());
            body.extend_from_slice(&[0xF0, 0]);
        }
        section(0x02, &body)
    }

    /// A payload packet on `pid`, optionally with a clock reference (27 MHz ticks).
    fn packet(pid: u16, clock: Option<u64>) -> [u8; PACKET] {
        let mut p = [0u8; PACKET];
        p[0] = SYNC;
        p[1] = (pid >> 8) as u8;
        p[2] = pid as u8;
        p[3] = 0x10;
        if let Some(v) = clock {
            let (base, ext) = (v / 300, v % 300);
            p[3] = 0x30;
            p[4] = 7;
            p[5] = 0x10;
            p[6..12].copy_from_slice(&[(base >> 25) as u8, (base >> 17) as u8, (base >> 9) as u8, (base >> 1) as u8, ((base & 1) << 7) as u8 | 0x7E | (ext >> 8) as u8, ext as u8]);
        }
        p
    }

    fn pids(stream: &[u8]) -> Vec<u16> {
        stream.chunks_exact(PACKET).map(|p| pid(p.try_into().unwrap())).collect()
    }

    #[test]
    fn a_programme_passes_alone_under_its_own_pat() {
        let mut f = ProgramFilter::new(3);
        let mut out = Vec::new();
        let feed = |f: &mut ProgramFilter, ps: &[[u8; PACKET]], out: &mut Vec<u8>| ps.iter().for_each(|p| f.push(p, out));
        // Before the tables, nothing.
        feed(&mut f, &[packet(0x31, None), packet(0x41, None)], &mut out);
        feed(&mut f, &packets(0, &pat(&[(0, 0x10), (3, 0x30), (4, 0x40)])), &mut out);
        feed(&mut f, &packets(0x40, &pmt(4, 0x41, &[(2, 0x41)])), &mut out);
        assert!(out.is_empty());
        // Its PMT: the PAT, then the PMT itself.
        feed(&mut f, &packets(0x30, &pmt(3, 0x31, &[(2, 0x31), (0x81, 0x34), (0x81, 0x35)])), &mut out);
        assert_eq!(pids(&out), vec![0, 0x30]);
        out.clear();
        feed(&mut f, &[packet(0x31, Some(0)), packet(0x41, None), packet(0x34, None), packet(0x1FFB, None), packet(0x44, None)], &mut out);
        feed(&mut f, &packets(0, &pat(&[(0, 0x10), (3, 0x30), (4, 0x40)])), &mut out);
        assert_eq!(pids(&out), vec![0x31, 0x34, 0x1FFB, 0]);
        // The rewritten PAT: one programme, CRC good, its continuity counter counting.
        let mut s = Sections::default();
        let pat_packet: [u8; PACKET] = out[3 * PACKET..].try_into().unwrap();
        assert_eq!(pat_packet[3] & 15, 1);
        let got = s.push(&pat_packet, &[0]);
        assert_eq!(got.len(), 1);
        let sec = &got[0].1;
        assert_eq!((sec[3], sec[4]), (0x12, 0x34));
        assert_eq!(&sec[8..12], &[0, 3, 0xE0, 0x30]);
        assert_eq!(sec.len(), 16);
        assert_eq!(f.pcr_pid(), Some(0x31));
    }

    #[test]
    fn clock_references_read_back() {
        for v in [0u64, 1, 299, 300, 27_000_000 * 3600 + 12_345, (1 << 33) * 300 - 1] {
            assert_eq!(pcr(&packet(0x31, Some(v))), Some(v));
        }
        assert_eq!(pcr(&packet(0x31, None)), None);
    }

    /// A recording of the whole multiplex: clock references 100 ms apart, each followed by 9 more
    /// packets. Played at its clock's pace, and again from the start, marked discontinuous.
    #[tokio::test(start_paused = true)]
    async fn played_at_the_pace_of_its_clock_and_again_from_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = recording_path(dir.path(), 14);
        let mut file = Vec::new();
        for i in 0..6u64 {
            file.extend_from_slice(&packet(0x31, Some(1_000_000 + i * 2_700_000)));
            for _ in 0..9 {
                file.extend_from_slice(&packet(0x34, None));
            }
        }
        std::fs::write(&path, &file).unwrap();
        assert_eq!(recordings(dir.path()), vec![TvRecording { rf: 14, bytes: file.len() as u64, programs: vec![] }]);
        let (tx, mut rx) = mpsc::channel(4);
        let start = Instant::now();
        let task = tokio::spawn(play(path, None, tx));
        let mut pieces = Vec::new();
        while pieces.len() < 8 {
            let piece = rx.recv().await.unwrap();
            pieces.push((start.elapsed(), piece));
        }
        drop(rx);
        task.await.unwrap().unwrap();
        // Each piece goes when its first clock reference is due, 100 ms apart throughout: the
        // recording's end and its start again are one piece.
        let ms: Vec<u128> = pieces.iter().map(|(t, _)| t.as_millis()).collect();
        assert_eq!(ms, vec![0, 100, 200, 300, 400, 500, 600, 700]);
        assert!(pieces[..5].iter().all(|(_, p)| p.len() == 10 * PACKET));
        let seam = &pieces[5].1;
        assert_eq!(seam.len(), 20 * PACKET);
        assert_eq!(seam[5] & 0x80, 0, "the recording's last clock reference");
        assert_eq!(seam[10 * PACKET + 5] & 0x80, 0x80, "its first again, after the jump");
    }
}
