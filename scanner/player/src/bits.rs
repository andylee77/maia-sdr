//! Reading a byte buffer as bits, most significant first. Reading past the end gives zeros and
//! marks the reader overrun, so a parser checks once at the end rather than at every field.

pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    overrun: bool,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Bits<'a> {
        Bits { data, pos: 0, overrun: false }
    }

    /// The next `n` bits (at most 32) as an unsigned number.
    pub fn get(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let mut v: u64 = 0;
        let mut left = n;
        while left > 0 {
            let byte = self.pos / 8;
            let Some(&b) = self.data.get(byte) else {
                self.overrun = true;
                self.pos += left as usize;
                return (v << left) as u32;
            };
            let off = (self.pos % 8) as u32;
            let take = left.min(8 - off);
            let bits = (u32::from(b) >> (8 - off - take)) & ((1 << take) - 1);
            v = (v << take) | u64::from(bits);
            self.pos += take as usize;
            left -= take;
        }
        v as u32
    }

    pub fn flag(&mut self) -> bool {
        self.get(1) == 1
    }

    /// The next `n` bits as a two's complement number.
    pub fn signed(&mut self, n: u32) -> i32 {
        let v = self.get(n);
        ((v << (32 - n)) as i32) >> (32 - n)
    }

    pub fn skip(&mut self, n: usize) {
        self.pos += n;
        if self.pos > self.data.len() * 8 {
            self.overrun = true;
        }
    }

    /// Bits read so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn overrun(&self) -> bool {
        self.overrun
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_cross_bytes_and_overrun_gives_zeros() {
        let mut b = Bits::new(&[0b1011_0001, 0b0111_1111]);
        assert_eq!(b.get(3), 0b101);
        assert_eq!(b.get(7), 0b100_0101);
        assert_eq!(b.signed(3), -1);
        assert!(!b.overrun());
        assert_eq!(b.get(8), 0b1110_0000);
        assert!(b.overrun());
    }
}
