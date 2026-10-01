//! Bit field access in SDRTrunk `BinaryMessage` style (bits are `u8` 0/1, first sent first).

/// `getInt(start, end - 1)`: bits `start..end` MSB first; bits past the end read as 0.
pub fn field(bits: &[u8], start: usize, end: usize) -> u32 {
    (start..end).fold(0, |acc, i| (acc << 1) | u32::from(get(bits, i)))
}

/// `get(index)`: false past the end, like a `BitSet`.
pub fn get(bits: &[u8], index: usize) -> bool {
    bits.get(index).map_or(false, |&b| b == 1)
}

/// `toHexString()`: one digit per started nibble, missing bits as 0.
pub fn hex(bits: &[u8]) -> String {
    (0..bits.len())
        .step_by(4)
        .map(|i| format!("{:X}", field(bits, i, i + 4)))
        .collect()
}

/// `getHex(int[], digits)` for a contiguous field: `%0<digits>X`.
pub fn hex_field(bits: &[u8], start: usize, end: usize, digits: usize) -> String {
    format!("{:0width$X}", field(bits, start, end), width = digits)
}
