//! Reading MPEG-2 video's bits: a reader that peeks up to 32 bits at once (past the end it reads
//! zeros, as the stuffing before a start code would), and variable length codes decoded through
//! a two-level lookup built from a table of (code, length, value).

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    /// The next `n` bits (at most 32) without taking them.
    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte = self.pos >> 3;
        let word = if byte + 8 <= self.data.len() {
            u64::from_be_bytes(self.data[byte..byte + 8].try_into().unwrap())
        } else {
            let mut b = [0u8; 8];
            if byte < self.data.len() {
                let n = self.data.len() - byte;
                b[..n].copy_from_slice(&self.data[byte..]);
            }
            u64::from_be_bytes(b)
        };
        ((word << (self.pos & 7)) >> (64 - n)) as u32
    }

    #[inline]
    pub fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    #[inline]
    pub fn get(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.skip(n);
        v
    }

    #[inline]
    pub fn flag(&mut self) -> bool {
        self.get(1) == 1
    }

    /// Bits left before the end (negative past it).
    pub fn left(&self) -> isize {
        (self.data.len() * 8) as isize - self.pos as isize
    }
}

#[derive(Clone, Copy)]
enum Entry<T: Copy> {
    Invalid,
    Leaf(T, u8),
    /// A second-level table: its start and how many more bits index it.
    Sub(u32, u8),
}

pub struct Vlc<T: Copy> {
    root: u8,
    entries: Vec<Entry<T>>,
}

impl<T: Copy> Vlc<T> {
    /// From (code, length, value), the root table indexed by `root` bits.
    pub fn new(codes: &[(u16, u8, T)], root: u8) -> Vlc<T> {
        let mut entries = vec![Entry::Invalid; 1 << root];
        // Codes no longer than the root fill every root entry they prefix.
        for &(code, len, v) in codes.iter().filter(|c| c.1 <= root) {
            let shift = root - len;
            let first = (code as usize) << shift;
            for e in &mut entries[first..first + (1 << shift)] {
                *e = Entry::Leaf(v, len);
            }
        }
        // Longer codes, grouped by their first `root` bits.
        let mut prefixes: Vec<usize> = codes.iter().filter(|c| c.1 > root).map(|c| (c.0 >> (c.1 - root)) as usize).collect();
        prefixes.sort_unstable();
        prefixes.dedup();
        for p in prefixes {
            let sub_bits = codes.iter().filter(|c| c.1 > root && (c.0 >> (c.1 - root)) as usize == p).map(|c| c.1 - root).max().unwrap();
            let start = entries.len();
            entries.resize(start + (1 << sub_bits), Entry::Invalid);
            for &(code, len, v) in codes.iter().filter(|c| c.1 > root && (c.0 >> (c.1 - root)) as usize == p) {
                let rest = (code as usize) & ((1 << (len - root)) - 1);
                let shift = sub_bits - (len - root);
                let first = start + (rest << shift);
                for e in &mut entries[first..first + (1 << shift)] {
                    *e = Entry::Leaf(v, len);
                }
            }
            entries[p] = Entry::Sub(start as u32, sub_bits);
        }
        Vlc { root, entries }
    }

    /// The next code's value, or None for a pattern the table does not hold.
    #[inline]
    pub fn read(&self, r: &mut Reader) -> Option<T> {
        match self.entries[r.peek(u32::from(self.root)) as usize] {
            Entry::Leaf(v, len) => {
                r.skip(u32::from(len));
                Some(v)
            }
            Entry::Sub(start, bits) => {
                let i = r.peek(u32::from(self.root + bits)) & ((1 << bits) - 1);
                match self.entries[start as usize + i as usize] {
                    Entry::Leaf(v, len) => {
                        r.skip(u32::from(len));
                        Some(v)
                    }
                    _ => None,
                }
            }
            Entry::Invalid => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_cross_bytes_and_read_zeros_past_the_end() {
        let mut r = Reader::new(&[0b1011_0001, 0b0111_1111, 0xFF]);
        assert_eq!(r.get(3), 0b101);
        assert_eq!(r.peek(7), 0b100_0101);
        r.skip(7);
        assert_eq!(r.get(14), 0x3FFF);
        assert_eq!(r.left(), 0);
        assert_eq!(r.get(4), 0);
        assert!(r.left() < 0);
    }

    #[test]
    fn short_and_long_codes_decode() {
        // 1 -> a, 01 -> b, 0001 1 -> c, 0000 0000 0011 -> d.
        let codes = [(0b1, 1, 'a'), (0b01, 2, 'b'), (0b00011, 5, 'c'), (0b0000_0000_0011, 12, 'd')];
        let v = Vlc::new(&codes, 4);
        let data = [0b1010_0011u8, 0b0000_0000, 0b0011_0000];
        let mut r = Reader::new(&data);
        assert_eq!(v.read(&mut r), Some('a'));
        assert_eq!(v.read(&mut r), Some('b'));
        assert_eq!(v.read(&mut r), Some('c'));
        assert_eq!(v.read(&mut r), Some('d'));
        assert_eq!(v.read(&mut r), None);
    }
}
