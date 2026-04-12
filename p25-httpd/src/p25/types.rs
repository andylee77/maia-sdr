//! P25 data types and protocol constants
//!
//! Reference: TIA-102.BAAA (P25 Common Air Interface)

/// 2-bit dibit symbol (P25 4FSK)
/// Mapping: +3 -> 01, +1 -> 00, -1 -> 10, -3 -> 11
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dibit(pub u8);

impl Dibit {
    pub fn value(self) -> u8 {
        self.0 & 0x03
    }
}

/// P25 Network Access Code (12 bits)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nac(pub u16);

impl Nac {
    pub fn new(val: u16) -> Self {
        Nac(val & 0xFFF)
    }
}

impl std::fmt::Display for Nac {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:03X}", self.0)
    }
}

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
        match val & 0xF {
            0x0 => Some(Self::Hdu),
            0x3 => Some(Self::Tdu),
            0x5 => Some(Self::Ldu1),
            0x7 => Some(Self::Tsdu),
            0xA => Some(Self::Ldu2),
            0xC => Some(Self::Pdu),
            0xF => Some(Self::TduLc),
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
    /// status dibits live at body raw positions {13, 49, 85, 121, ...}
    /// (= 13 + 36*k for k >= 0). This is because SDRTrunk's
    /// `mStatusSymbolDibitCounter` is set to 21 at NID-detect and
    /// incremented by 1 each iteration, so the first body iteration
    /// has counter 23 and the counter hits 36 at body pos 13 (then
    /// resets to 0 and counts back up to 36 every 36 iterations).
    /// Use `is_body_status_dibit(body_pos)` to check.
    ///
    /// **Phase 7C correction (2026-04-11):** the previous values for
    /// HDU=324, TDU=0, LDU1=792, LDU2=792, TDULC=168 were never tested
    /// because Phase 6 only handled TSDU. Phase 7C is the first time
    /// LDU lengths matter (for IMBE frame extraction from voice
    /// channels), so they get fixed here. Verify against the SDRTrunk
    /// table above before changing again.
    pub fn length_dibits(self) -> usize {
        match self {
            Self::Hdu => 339,    // 648 bits + 10 null + 10 body status (was 324)
            Self::Tdu => 15,     // 0 bits + 14 null + 1 body status (was 0)
            Self::Ldu1 => 807,   // 1568 bits + 0 null + 23 body status (was 792)
            Self::Tsdu => 123,   // unchanged ✓ (was 123, validated in Phase 6F.2i)
            Self::Ldu2 => 807,   // 1568 bits + 0 null + 23 body status (was 792)
            Self::Pdu => 288,    // not yet validated
            Self::TduLc => 159,  // 288 bits + 20 null + 5 body status (was 168)
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
            Self::Pdu => 288,    // not yet validated
            Self::TduLc => 154,
        }
    }
}

/// Returns true if `body_raw_pos` (0-indexed dibit position within the
/// body, INCLUDING status dibits) is a status dibit.
///
/// The body status pattern is universal across all P25 Phase 1 data
/// unit types (HDU, TDU, LDU1, LDU2, TSDU, TDU_LC, PDU). Status dibits
/// live at body raw positions {13, 49, 85, 121, ...} = 13 + 36*k for
/// k = 0, 1, 2, ...
///
/// Derivation: SDRTrunk's `P25P1MessageFramer` sets
/// `mStatusSymbolDibitCounter = 21` at NID-detect. The first body
/// iteration sees counter 23 (after two pre-iterations). The counter
/// increments by 1 each iteration and a status dibit is dropped when
/// counter == 36, resetting to 0. So the first body status drop is at
/// body pos (36 - 23) = 13, and subsequent drops are at 13 + 36*k.
#[inline]
pub fn is_body_status_dibit(body_raw_pos: usize) -> bool {
    body_raw_pos >= 13 && (body_raw_pos - 13) % 36 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_status_pattern_matches_tsdu() {
        // TSDU body has 4 status dibits at {13, 49, 85, 121} per the
        // existing Phase 6F.2i validation. Verify the universal helper
        // returns the same pattern.
        let tsdu_status: Vec<usize> = (0..123)
            .filter(|p| is_body_status_dibit(*p))
            .collect();
        assert_eq!(tsdu_status, vec![13, 49, 85, 121]);
    }

    #[test]
    fn body_status_count_matches_sdrtrunk_table() {
        // Cross-check the universal status pattern against the SDRTrunk
        // P25P1DataUnitID statusDibits field minus the in-NID status (1).
        let cases = [
            (DataUnit::Hdu, 339, 10),
            (DataUnit::Tdu, 15, 1),
            (DataUnit::Ldu1, 807, 23),
            (DataUnit::Tsdu, 123, 4),
            (DataUnit::Ldu2, 807, 23),
            (DataUnit::TduLc, 159, 5),
        ];
        for (du, expected_len, expected_n_status) in cases {
            assert_eq!(
                du.length_dibits(),
                expected_len,
                "{:?} length_dibits", du
            );
            let n_status = (0..expected_len)
                .filter(|p| is_body_status_dibit(*p))
                .count();
            assert_eq!(
                n_status, expected_n_status,
                "{:?} body status count", du
            );
            assert_eq!(
                du.data_dibits(),
                expected_len - expected_n_status,
                "{:?} data_dibits = length - n_status", du
            );
        }
    }
}

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

/// NID (Network Identifier) - 64 bits (48 transmitted dibits after frame sync)
/// Contains NAC (12 bits) + DUID (4 bits) + Golay parity
///
/// Frame sync pattern: 48 dibits = 96 bits
/// 0x5575F5FF77FF (TIA-102.BAAA Table 7-1)
pub const FRAME_SYNC_DIBITS: [u8; 48] = [
    0, 1, 0, 1, 0, 1, 1, 1, 0, 1, 0, 1, // 0x5575
    1, 1, 1, 1, 0, 1, 0, 1, 1, 1, 1, 1, // F5FF
    0, 1, 1, 1, 0, 1, 1, 1, 1, 1, 1, 1, // 77FF
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // (continued)
];

/// Frame sync as 64-bit value for fast correlation
/// Each dibit maps: 01->1, 00->0, 10->2, 11->3
/// But for correlation we use the raw dibit bits
pub const FRAME_SYNC_BITS: u64 = 0x5575F5FF77FF;
