//! MPEG-2 transport stream sections: the packets of chosen PIDs reassembled into PSI and PSIP
//! sections, each kept only when its CRC-32 checks.

use std::collections::HashMap;

/// The PIDs PSIP needs: the PAT's and the ATSC base PID (MGT, VCT, STT).
pub const PAT_PID: u16 = 0x0000;
pub const PSIP_PID: u16 = 0x1FFB;

/// MPEG-2's CRC-32 (0x04C11DB7, from all ones, not reflected); a section with its CRC gives 0.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

pub fn pid(packet: &[u8; 188]) -> u16 {
    (u16::from(packet[1] & 0x1F) << 8) | u16::from(packet[2])
}

#[derive(Default)]
pub struct Sections {
    /// Each PID's section so far.
    open: HashMap<u16, Vec<u8>>,
}

impl Sections {
    /// The sections `packet` completes (on the PIDs in `pids`). A packet missed between two
    /// leaves a section whose CRC fails: it is dropped.
    pub fn push(&mut self, packet: &[u8; 188], pids: &[u16]) -> Vec<(u16, Vec<u8>)> {
        let pid = pid(packet);
        if !pids.contains(&pid) {
            return Vec::new();
        }
        let start = packet[1] & 0x40 != 0;
        let control = (packet[3] >> 4) & 3;
        if control & 1 == 0 {
            return Vec::new();
        }
        let mut i = 4;
        if control & 2 != 0 {
            i += 1 + packet[4] as usize;
        }
        if i >= 188 {
            return Vec::new();
        }
        let payload = &packet[i..];
        let mut out = Vec::new();
        if start {
            let ptr = payload[0] as usize;
            let rest = &payload[1..];
            if let Some(buf) = self.open.get_mut(&pid) {
                buf.extend_from_slice(&rest[..ptr.min(rest.len())]);
                let done = std::mem::take(buf);
                out.extend(split(pid, &done));
            }
            self.open.insert(pid, rest.get(ptr..).unwrap_or(&[]).to_vec());
        } else if let Some(buf) = self.open.get_mut(&pid) {
            buf.extend_from_slice(payload);
        }
        // Sections complete within this packet.
        if let Some(buf) = self.open.get_mut(&pid) {
            let mut used = 0;
            while let Some(len) = section_len(&buf[used..]) {
                if buf.len() - used < len {
                    break;
                }
                let sec = &buf[used..used + len];
                if crc32(sec) == 0 {
                    out.push((pid, sec.to_vec()));
                }
                used += len;
            }
            buf.drain(..used);
        }
        out
    }
}

/// A section's whole length from its header; none at stuffing (0xFF) or too few bytes.
fn section_len(b: &[u8]) -> Option<usize> {
    if b.len() < 3 || b[0] == 0xFF {
        return None;
    }
    Some(3 + ((usize::from(b[1] & 0x0F) << 8) | usize::from(b[2])))
}

fn split(pid: u16, b: &[u8]) -> Vec<(u16, Vec<u8>)> {
    let mut out = Vec::new();
    let mut used = 0;
    while let Some(len) = section_len(&b[used..]) {
        if b.len() - used < len {
            break;
        }
        let sec = &b[used..used + len];
        if crc32(sec) == 0 {
            out.push((pid, sec.to_vec()));
        }
        used += len;
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A section with its CRC.
    pub fn section(table: u8, body: &[u8]) -> Vec<u8> {
        let len = body.len() + 4;
        let mut s = vec![table, 0xB0 | ((len >> 8) as u8 & 0x0F), len as u8];
        s.extend_from_slice(body);
        let crc = crc32(&s);
        s.extend_from_slice(&crc.to_be_bytes());
        s
    }

    /// `data` cut into packets on `pid`, the first with the pointer field.
    pub fn packets(pid: u16, data: &[u8]) -> Vec<[u8; 188]> {
        let mut out = Vec::new();
        let mut rest = data.to_vec();
        let mut first = true;
        while !rest.is_empty() || first {
            let mut p = [0xFFu8; 188];
            p[0] = 0x47;
            p[1] = (if first { 0x40 } else { 0 }) | (pid >> 8) as u8;
            p[2] = pid as u8;
            p[3] = 0x10;
            let mut i = 4;
            if first {
                p[4] = 0;
                i = 5;
            }
            let n = (188 - i).min(rest.len());
            p[i..i + n].copy_from_slice(&rest[..n]);
            rest.drain(..n);
            out.push(p);
            first = false;
        }
        out
    }

    #[test]
    fn a_section_across_packets_comes_out_whole_and_checked() {
        let body: Vec<u8> = (0..400u32).map(|i| i as u8).collect();
        let sec = section(0xC8, &body);
        let mut s = Sections::default();
        let mut got = Vec::new();
        for p in packets(PSIP_PID, &sec) {
            got.extend(s.push(&p, &[PSIP_PID]));
        }
        assert_eq!(got, vec![(PSIP_PID, sec.clone())]);
        // A corrupted byte fails the CRC.
        let mut bad = sec.clone();
        bad[100] ^= 1;
        let mut s = Sections::default();
        let n: usize = packets(PSIP_PID, &bad).iter().map(|p| s.push(p, &[PSIP_PID]).len()).sum();
        assert_eq!(n, 0);
        assert_eq!(crc32(b"123456789"), 0x0376_E6E7, "CRC-32/MPEG-2's check value");
    }
}
