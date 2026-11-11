//! A ZIP archive written as a stream, its files stored as they are (no compression): each file's
//! header is made as its bytes are handed over, and the directory comes at the end. Nothing is
//! kept of a file but its directory entry, so an archive larger than memory can be sent as it is
//! made. The ZIP64 records are written when the archive needs them: 65,535 files or more, or a
//! directory or an offset past 4 GiB. A file itself stays under 4 GiB.

use super::time::utc_fields;

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const END: u32 = 0x0605_4b50;
const END64: u32 = 0x0606_4b50;
const END64_LOCATOR: u32 = 0x0706_4b50;
/// File names are UTF-8.
const FLAGS: u16 = 0x0800;
/// The ZIP versions a reader needs: 2.0 for a stored file, 4.5 for ZIP64.
const V20: u16 = 20;
const V45: u16 = 45;
/// The extra field with a file's time as Unix seconds (the DOS fields have no zone).
const EXTRA_TIME: u16 = 0x5455;
const EXTRA_ZIP64: u16 = 0x0001;

const fn crc_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut k = 1;
        while k < 8 {
            let p = t[k - 1][i];
            t[k][i] = (p >> 8) ^ t[0][(p & 0xFF) as usize];
            k += 1;
        }
        i += 1;
    }
    t
}

static CRC: [[u32; 256]; 8] = crc_tables();

/// CRC-32 (IEEE 802.3, reflected), which ZIP checks a file by; eight bytes a step.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    let mut eights = data.chunks_exact(8);
    for b in &mut eights {
        let lo = c ^ u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let hi = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        c = CRC[7][(lo & 0xFF) as usize]
            ^ CRC[6][((lo >> 8) & 0xFF) as usize]
            ^ CRC[5][((lo >> 16) & 0xFF) as usize]
            ^ CRC[4][(lo >> 24) as usize]
            ^ CRC[3][(hi & 0xFF) as usize]
            ^ CRC[2][((hi >> 8) & 0xFF) as usize]
            ^ CRC[1][((hi >> 16) & 0xFF) as usize]
            ^ CRC[0][(hi >> 24) as usize];
    }
    for &b in eights.remainder() {
        c = CRC[0][((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

struct Entry {
    name: String,
    crc: u32,
    size: u32,
    offset: u64,
    time: u16,
    date: u16,
    unix_s: u32,
}

/// The archive being written: where its next byte goes, and the directory so far.
#[derive(Default)]
pub struct ZipWriter {
    offset: u64,
    entries: Vec<Entry>,
}

/// A file's time in the DOS fields, UTC: (time, date). A time before 1980 is 1980's first day.
fn dos_time(unix_s: u64) -> (u16, u16) {
    let (year, month, day, hour, minute, second) = utc_fields(unix_s);
    if year < 1980 {
        return (0, 1 << 5 | 1);
    }
    let year = (year - 1980).min(127) as u16;
    ((hour << 11 | minute << 5 | second / 2) as u16, year << 9 | (month as u16) << 5 | day as u16)
}

fn put16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// The time's extra field: one flag (the modification time is here), then the time.
fn put_time(out: &mut Vec<u8>, unix_s: u32) {
    put16(out, EXTRA_TIME);
    put16(out, 5);
    out.push(1);
    put32(out, unix_s);
}

impl ZipWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The header to send before file `name`'s bytes `data` (which the caller sends next, as they
    /// are), last changed at `unix_s`. None for a file the format cannot hold: 4 GiB or more, or
    /// a name past 65,535 bytes; nothing is recorded of it then.
    pub fn file(&mut self, name: &str, unix_s: u64, data: &[u8]) -> Option<Vec<u8>> {
        let size = u32::try_from(data.len()).ok().filter(|&n| n < u32::MAX)?;
        let name_len = u16::try_from(name.len()).ok()?;
        let (time, date) = dos_time(unix_s);
        let unix_s = u32::try_from(unix_s).unwrap_or(u32::MAX);
        let crc = crc32(data);
        let mut out = Vec::with_capacity(30 + name.len() + 9);
        put32(&mut out, LOCAL);
        put16(&mut out, V20);
        put16(&mut out, FLAGS);
        put16(&mut out, 0);
        put16(&mut out, time);
        put16(&mut out, date);
        put32(&mut out, crc);
        put32(&mut out, size);
        put32(&mut out, size);
        put16(&mut out, name_len);
        put16(&mut out, 9);
        out.extend_from_slice(name.as_bytes());
        put_time(&mut out, unix_s);
        self.entries.push(Entry { name: name.to_string(), crc, size, offset: self.offset, time, date, unix_s });
        self.offset += out.len() as u64 + u64::from(size);
        Some(out)
    }

    /// Files written so far.
    #[cfg(test)]
    pub fn files(&self) -> usize {
        self.entries.len()
    }

    /// The directory and the archive's end, to send last.
    pub fn finish(self) -> Vec<u8> {
        let start = self.offset;
        let mut out = Vec::with_capacity(self.entries.iter().map(|e| 46 + e.name.len() + 9 + 12).sum::<usize>() + 98);
        for e in &self.entries {
            // An offset past 4 GiB goes in the ZIP64 extra field.
            let far = e.offset >= u64::from(u32::MAX);
            put32(&mut out, CENTRAL);
            put16(&mut out, V45);
            put16(&mut out, if far { V45 } else { V20 });
            put16(&mut out, FLAGS);
            put16(&mut out, 0);
            put16(&mut out, e.time);
            put16(&mut out, e.date);
            put32(&mut out, e.crc);
            put32(&mut out, e.size);
            put32(&mut out, e.size);
            put16(&mut out, e.name.len() as u16);
            put16(&mut out, 9 + if far { 12 } else { 0 });
            put16(&mut out, 0);
            put16(&mut out, 0);
            put16(&mut out, 0);
            put32(&mut out, 0);
            put32(&mut out, if far { u32::MAX } else { e.offset as u32 });
            out.extend_from_slice(e.name.as_bytes());
            put_time(&mut out, e.unix_s);
            if far {
                put16(&mut out, EXTRA_ZIP64);
                put16(&mut out, 8);
                put64(&mut out, e.offset);
            }
        }
        let size = out.len() as u64;
        let count = self.entries.len() as u64;
        if count >= u64::from(u16::MAX) || size >= u64::from(u32::MAX) || start >= u64::from(u32::MAX) {
            let end64 = start + size;
            put32(&mut out, END64);
            put64(&mut out, 44);
            put16(&mut out, V45);
            put16(&mut out, V45);
            put32(&mut out, 0);
            put32(&mut out, 0);
            put64(&mut out, count);
            put64(&mut out, count);
            put64(&mut out, size);
            put64(&mut out, start);
            put32(&mut out, END64_LOCATOR);
            put32(&mut out, 0);
            put64(&mut out, end64);
            put32(&mut out, 1);
        }
        let count16 = count.min(u64::from(u16::MAX)) as u16;
        put32(&mut out, END);
        put16(&mut out, 0);
        put16(&mut out, 0);
        put16(&mut out, count16);
        put16(&mut out, count16);
        put32(&mut out, size.min(u64::from(u32::MAX)) as u32);
        put32(&mut out, start.min(u64::from(u32::MAX)) as u32);
        put16(&mut out, 0);
        out
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    /// An archive read back from its end, as an unzip does: each file's name, time (Unix
    /// seconds) and bytes, every header checked against the directory and every CRC against the
    /// bytes.
    pub fn read(zip: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
        let end = zip.len() - 22;
        assert_eq!(u32_at(zip, end), END, "the archive ends with its end record");
        let (mut count, mut size, mut start) = (u64::from(u16_at(zip, end + 10)), u64::from(u32_at(zip, end + 12)), u64::from(u32_at(zip, end + 16)));
        if count == u64::from(u16::MAX) || size == u64::from(u32::MAX) || start == u64::from(u32::MAX) {
            let locator = end - 20;
            assert_eq!(u32_at(zip, locator), END64_LOCATOR);
            let end64 = u64_at(zip, locator + 8) as usize;
            assert_eq!(u32_at(zip, end64), END64);
            assert_eq!(u64_at(zip, end64 + 4), 44);
            (count, size, start) = (u64_at(zip, end64 + 32), u64_at(zip, end64 + 40), u64_at(zip, end64 + 48));
            assert_eq!(end64 as u64, start + size, "the ZIP64 end follows the directory");
        } else {
            assert_eq!(end as u64, start + size, "the end follows the directory");
        }
        let mut at = start as usize;
        let mut out = Vec::new();
        for _ in 0..count {
            assert_eq!(u32_at(zip, at), CENTRAL);
            let (crc, len) = (u32_at(zip, at + 16), u32_at(zip, at + 24) as usize);
            assert_eq!(u32_at(zip, at + 20) as usize, len, "stored: both sizes are the file's");
            let (name_len, extra_len) = (u16_at(zip, at + 28) as usize, u16_at(zip, at + 30) as usize);
            let name = String::from_utf8(zip[at + 46..at + 46 + name_len].to_vec()).unwrap();
            let extra = &zip[at + 46 + name_len..at + 46 + name_len + extra_len];
            assert_eq!((u16_at(extra, 0), u16_at(extra, 2), extra[4]), (EXTRA_TIME, 5, 1));
            let unix_s = u32_at(extra, 5);
            let mut offset = u64::from(u32_at(zip, at + 42));
            if offset == u64::from(u32::MAX) {
                assert_eq!((u16_at(extra, 9), u16_at(extra, 11)), (EXTRA_ZIP64, 8));
                offset = u64_at(extra, 13);
            }
            let local = offset as usize;
            assert_eq!(u32_at(zip, local), LOCAL);
            assert_eq!((u16_at(zip, local + 6), u16_at(zip, local + 8)), (FLAGS, 0), "UTF-8 names, stored");
            assert_eq!((u32_at(zip, local + 14), u32_at(zip, local + 18) as usize, u32_at(zip, local + 22) as usize), (crc, len, len));
            assert_eq!((u16_at(zip, local + 10), u16_at(zip, local + 12)), (u16_at(zip, at + 12), u16_at(zip, at + 14)), "one time in both headers");
            let (local_name, local_extra) = (u16_at(zip, local + 26) as usize, u16_at(zip, local + 28) as usize);
            assert_eq!(&zip[local + 30..local + 30 + local_name], name.as_bytes());
            let data = zip[local + 30 + local_name + local_extra..][..len].to_vec();
            assert_eq!(crc32(&data), crc, "{name}: its bytes are what its CRC says");
            out.push((name, unix_s, data));
            at += 46 + name_len + extra_len;
        }
        assert_eq!(at as u64, start + size, "the directory is as long as its end says");
        out
    }

    #[test]
    fn crc32_is_zips() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926, "CRC-32's check value");
        // Past the eight-byte steps, with a tail.
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
    }

    #[test]
    fn a_time_in_the_dos_fields() {
        // 2026-11-11 13:29:53 UTC.
        let (time, date) = dos_time(1_794_403_793);
        assert_eq!((date >> 9, date >> 5 & 15, date & 31), (2026 - 1980, 11, 11));
        assert_eq!((time >> 11, time >> 5 & 63, (time & 31) * 2), (13, 29, 52));
        assert_eq!(dos_time(0), (0, 1 << 5 | 1), "before 1980: its first day");
    }

    #[test]
    fn an_archive_reads_back() {
        let mut zip = ZipWriter::new();
        let mut out = Vec::new();
        let files: [(&str, u64, &[u8]); 3] =
            [("2026-11-11/a.wav", 1_794_403_793, b"RIFF....WAVE"), ("2026-11-11/empty.wav", 1_794_403_800, b""), ("récords.csv", 1_794_403_900, b"file,tg\n")];
        for (name, at, data) in files {
            out.extend(zip.file(name, at, data).unwrap());
            out.extend_from_slice(data);
        }
        assert_eq!(zip.files(), 3);
        out.extend(zip.finish());
        let back = read(&out);
        assert_eq!(back.len(), 3);
        for ((name, at, data), (got_name, got_at, got)) in files.iter().zip(&back) {
            assert_eq!((name, *at as u32, *data), (&got_name.as_str(), *got_at, got.as_slice()));
        }
        // No ZIP64 record in a small archive: the end record is the last 22 bytes, and before it
        // is the last directory entry, not a locator.
        assert_ne!(u32_at(&out, out.len() - 42), END64_LOCATOR);
    }

    #[test]
    fn many_files_get_the_zip64_end() {
        let mut zip = ZipWriter::new();
        let mut out = Vec::new();
        let n = 70_000;
        for i in 0..n {
            let data = (i as u32).to_le_bytes();
            out.extend(zip.file(&format!("d/{i}.wav"), 1_794_403_793 + i, &data).unwrap());
            out.extend_from_slice(&data);
        }
        out.extend(zip.finish());
        let end = out.len() - 22;
        assert_eq!(u16_at(&out, end + 10), u16::MAX, "the short count says: look in the ZIP64 end");
        let back = read(&out);
        assert_eq!(back.len(), n as usize);
        assert_eq!(back[69_999], ("d/69999.wav".to_string(), 1_794_403_793 + 69_999, 69_999u32.to_le_bytes().to_vec()));
    }

    #[test]
    fn an_offset_past_4_gib_goes_in_the_zip64_field() {
        // Not 4 GiB of bytes: the writer is told where it is.
        let mut zip = ZipWriter { offset: 5 << 30, entries: Vec::new() };
        let head = zip.file("late.wav", 1_794_403_793, b"abc").unwrap();
        let tail = zip.finish();
        assert_eq!(u32_at(&tail, 0), CENTRAL);
        assert_eq!(u16_at(&tail, 6), V45, "a ZIP64 reader is needed for it");
        assert_eq!(u32_at(&tail, 42), u32::MAX);
        let extra = &tail[46 + 8..];
        assert_eq!((u16_at(extra, 9), u16_at(extra, 11), u64_at(extra, 13)), (EXTRA_ZIP64, 8, 5 << 30));
        // The directory starts past 4 GiB: the ZIP64 end says where.
        let end64 = 46 + 8 + 9 + 12;
        assert_eq!(u32_at(&tail, end64), END64);
        assert_eq!(u64_at(&tail, end64 + 48), (5 << 30) + head.len() as u64 + 3);
        assert_eq!(u32_at(&tail, tail.len() - 22 + 16), u32::MAX);
    }

    #[test]
    fn a_name_too_long_is_refused_and_leaves_nothing() {
        let mut zip = ZipWriter::new();
        assert!(zip.file(&"n".repeat(70_000), 0, b"x").is_none());
        assert_eq!(zip.files(), 0);
        assert_eq!(zip.finish().len(), 22);
    }
}
