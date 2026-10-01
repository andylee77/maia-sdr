//! P25 data types and protocol constants
//!
//! Reference: TIA-102.BAAA (P25 Common Air Interface)

/// P25 Data Unit ID (4 bits, from NID)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataUnit {
    /// Header Data Unit (0x0)
    Hdu,
    /// Terminator Data Unit (0x3)
    Tdu,
    /// Logical Link Data Unit 1 (0x5)
    Ldu1,
    /// Trunking Signaling Data Unit (0x7)
    Tsdu,
    /// Logical Link Data Unit 2 (0xA)
    Ldu2,
    /// Packet Data Unit (0xC)
    Pdu,
    /// Terminator Data Unit with Link Control (0xF)
    TduLc,
}

impl DataUnit {
    pub fn from_duid(val: u8) -> Option<Self> {
        use super::wire::{
            DUID_HDU, DUID_LDU1, DUID_LDU2, DUID_PDU, DUID_TDU,
            DUID_TDU_LC, DUID_TSDU,
        };
        match val & 0xF {
            DUID_HDU    => Some(Self::Hdu),
            DUID_TDU    => Some(Self::Tdu),
            DUID_LDU1   => Some(Self::Ldu1),
            DUID_TSDU   => Some(Self::Tsdu),
            DUID_LDU2   => Some(Self::Ldu2),
            DUID_PDU    => Some(Self::Pdu),
            DUID_TDU_LC => Some(Self::TduLc),
            _ => None,
        }
    }

    /// Number of body raw dibits (excluding sync + NID, INCLUDING the
    /// in-body status dibits).
    ///
    /// All values cross-checked against SDRTrunk
    /// `P25P1DataUnitID.java`'s constructor formula
    /// `mElapsedDibitLength = 56 + (messageLength / 2) + statusDibits + (nullBits / 2)`
    /// where `56 = 24 sync dibits + 32 NID data dibits` (the in-NID
    /// status dibit at NID position 11 is counted in the per-frame
    /// `statusDibits` field, not in the 32-dibit NID data count). Body
    /// raw = elapsed - 24 sync - 33 NID raw (= 32 NID data + 1 NID
    /// status). Body status count = per-frame status - 1 (the NID
    /// status dibit). Body data count = body raw - body status count.
    ///
    /// | DUID  | msgLen | statDib | nullBit | elapsed | body raw | body data | body status |
    /// |-------|--------|---------|---------|---------|----------|-----------|-------------|
    /// | HDU   |  648   |   11    |   10    |   396   |   339    |    329    |     10      |
    /// | TDU   |    0   |    2    |   28    |    72   |    15    |     14    |      1      |
    /// | LDU1  | 1568   |   24    |    0    |   864   |   807    |    784    |     23      |
    /// | TSDU  |  196   |    5    |   42    |   180   |   123    |    119    |      4      |
    /// | LDU2  | 1568   |   24    |    0    |   864   |   807    |    784    |     23      |
    /// | TDULC |  288   |    6    |   20    |   216   |   159    |    154    |      5      |
    ///
    /// **Body status dibit pattern is universal across all DUIDs:**
    /// status dibits live at body raw positions {14, 50, 86, 122, ...}
    /// (= 14 + 36*k): every 36th dibit of the frame (frame dibits 35,
    /// 71, ...), the body starting at frame dibit 57. The last raw dibit
    /// of every extent above is a status dibit. Use
    /// `is_body_status_dibit(body_pos)` to check. Verify against the
    /// SDRTrunk table above before changing a length.
    pub fn length_dibits(self) -> usize {
        match self {
            Self::Hdu => 339,    // 648 bits + 10 null + 10 body status
            Self::Tdu => 15,     // 0 bits + 14 null + 1 body status
            Self::Ldu1 => 807,   // 1568 bits + 0 null + 23 body status
            Self::Tsdu => 123,   // one block: 196 bits + 42 null + 4 body status
            Self::Ldu2 => 807,   // 1568 bits + 0 null + 23 body status
            // The header block (98 data + 3 status dibits); the decoder
            // reads the data blocks it announces after it.
            Self::Pdu => 101,
            Self::TduLc => 159,  // 288 bits + 20 null + 5 body status
        }
    }

    /// Number of body data dibits (excluding both sync+NID AND body
    /// status dibits). This is the post-status-strip count -- the
    /// number of dibits the IMBE / TSBK / LC extractor sees.
    pub fn data_dibits(self) -> usize {
        match self {
            Self::Hdu => 329,
            Self::Tdu => 14,
            Self::Ldu1 => 784,
            Self::Tsdu => 119,
            Self::Ldu2 => 784,
            Self::Pdu => 98,
            Self::TduLc => 154,
        }
    }
}

/// Returns true if `body_raw_pos` (0-indexed dibit position within the
/// body, INCLUDING status dibits) is a status dibit.
///
/// The body status pattern is universal across all P25 Phase 1 data
/// unit types (HDU, TDU, LDU1, LDU2, TSDU, TDU_LC, PDU). Status dibits
/// are every 36th dibit of the frame: frame dibits 35, 71, 107, ...
/// (frame dibit 35 is the one inside the NID). The body starts at frame
/// dibit 57 (24 sync + 33 NID), so body raw positions 14 + 36*k. One
/// dibit off still decodes most TSBKs (the 1/2-rate trellis corrects
/// it) but no 3/4-rate packet-data block.
#[inline]
pub fn is_body_status_dibit(body_raw_pos: usize) -> bool {
    body_raw_pos >= 14 && (body_raw_pos - 14) % 36 == 0
}
#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;


/// Talkgroup identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Talkgroup(pub u16);

impl std::fmt::Display for Talkgroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:05}", self.0)
    }
}

/// Radio unit identifier (24 bits)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RadioId(pub u32);

impl std::fmt::Display for RadioId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:08}", self.0)
    }
}

/// Logical channel number (maps to frequency via IDEN_UP table)
/// Format: 4-bit identifier | 12-bit channel number
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel(pub u16);

impl Channel {
    /// Frequency band identifier (top 4 bits)
    pub fn identifier(self) -> u8 {
        (self.0 >> 12) as u8
    }

    /// Channel number within band (bottom 12 bits)
    pub fn number(self) -> u16 {
        self.0 & 0xFFF
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.identifier(), self.number())
    }
}

