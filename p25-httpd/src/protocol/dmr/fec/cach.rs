//! Common Announcement Channel: port of SDRTrunk `module/decode/dmr/message/CACH.java`.
//!
//! The 24-bit CACH precedes each outbound burst. Deinterleaved, bits 0..7 are
//! the TACT (AT, TC, LCSS, Hamming(7,4) parity) and bits 7..24 a 17-bit short
//! LC fragment.

use super::get_int;

/// Hamming(7,4) lookup indexed by the 7-bit TACT: 0..6 = bit in error,
/// -1 = no errors, -2 = uncorrectable. Codewords with LCSS 00 count as invalid
/// (SDRTrunk `createHamming7_4BitErrorMap()`).
pub const BIT_ERROR_INDEXES: [i8; 128] = [
    -2, -2, -2, -2, -2, -2, -2, -2, -2, 5, 6, -1, 1, 2, 0, 4, -2, 1, 4, 0, 5, 3, -1, 6, 0, 4, 1, 2,
    6, -1, 3, 5, -2, -2, -2, -2, -2, -2, -2, -2, 4, 0, 2, 1, -1, 6, 5, -2, 6, -1, 3, 5, 0, 4, 1,
    -2, 5, 3, -1, 6, 2, 1, 4, 0, -2, -2, -2, -2, -2, -2, -2, -2, 2, 1, 4, 0, 5, -2, -1, 6, 3, 5, 6,
    -1, 1, -2, 0, 4, -1, 6, 5, 3, 4, 0, 2, 1, -2, -2, -2, -2, -2, -2, -2, -2, 6, -1, -2, 5, 0, 4,
    1, 2, 4, 0, -2, 1, -1, 6, 5, 3, 1, 2, 0, 4, 3, 5, 6, -1,
];

const CACH_MESSAGE_LENGTH: usize = 24;

/// Deinterleave: decoded bit `x` is transmitted bit `INTERLEAVE_MATRIX[x]`.
pub const INTERLEAVE_MATRIX: [usize; CACH_MESSAGE_LENGTH] = [
    0, 4, 8, 12, 14, 18, 22, 1, 2, 3, 5, 6, 7, 9, 10, 11, 13, 15, 16, 17, 19, 20, 21, 23,
];

const INBOUND_CHANNEL_ACCESS_TYPE: usize = 0;
const OUTBOUND_BURST_TIMESLOT: usize = 1;
/// Parity contributions of the four TACT data bits (Hamming(7,4) bits 4..6).
pub const CHECKSUMS: [u32; 4] = [5, 7, 6, 3];
const PAYLOAD_START: usize = 7;

/// SDRTrunk CACH (deinterleaved, Hamming(7,4)-checked TACT + payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cach {
    /// TACT Hamming(7,4) correctable (SDRTrunk `bitError != -2`).
    pub valid: bool,
    /// Inbound channel access type bit (AT): true = BUSY.
    pub busy: bool,
    /// Outbound timeslot of the following burst, 1 or 2 (TC bit).
    pub timeslot: u8,
    /// Link control start/stop, 2 bits (SDRTrunk `LCSS` ordinal).
    pub lcss: u8,
    /// Short-LC fragment: bits 7..24 of the deinterleaved CACH.
    pub payload: [u8; 17],
}

/// Decodes the CACH from the first 24 bits of a 288-bit burst (transmission order).
/// Ports `CACH.getCACH()` plus its field accessors.
pub fn decode(burst: &[u8]) -> Cach {
    let mut message = deinterleave(burst);
    let bit_error = BIT_ERROR_INDEXES[get_int(&message[0..7]) as usize];

    if bit_error >= 0 {
        message[bit_error as usize] ^= 1;
    }

    let mut payload = [0u8; 17];
    payload.copy_from_slice(&message[PAYLOAD_START..CACH_MESSAGE_LENGTH]);

    Cach {
        valid: bit_error != -2,
        busy: message[INBOUND_CHANNEL_ACCESS_TYPE] == 1,
        timeslot: if message[OUTBOUND_BURST_TIMESLOT] == 1 {
            2
        } else {
            1
        },
        lcss: get_int(&message[2..4]) as u8,
        payload,
    }
}

/// Deinterleaves the first 24 transmitted bits (the loop in `CACH.getCACH()`).
pub fn deinterleave(burst: &[u8]) -> [u8; CACH_MESSAGE_LENGTH] {
    let mut message = [0u8; CACH_MESSAGE_LENGTH];
    for (x, bit) in message.iter_mut().enumerate() {
        *bit = burst[INTERLEAVE_MATRIX[x]];
    }
    message
}

/// Hamming(7,4) residual of a deinterleaved CACH, 0 = valid. Ports `CACH.getCrcChecksum()`.
pub fn get_crc_checksum(message: &[u8]) -> u32 {
    let mut checksum = get_int(&message[4..7]);
    for (x, parity) in CHECKSUMS.iter().enumerate() {
        if message[x] == 1 {
            checksum ^= parity;
        }
    }
    checksum
}

#[cfg(test)]
#[path = "cach_tests.rs"]
mod tests;
