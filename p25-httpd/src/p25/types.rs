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

    /// Number of dibits in this data unit (excluding NID).
    ///
    /// **Phase 6F.2g (2026-04-11) note for Tsdu:** Final value 122 after
    /// off-by-one correction from 6F.2f (which was 123). The math:
    /// SDRTrunk's `P25P1DataUnitID.TRUNKING_SIGNALING_BLOCK_1` declares
    /// `messageLength = 196 + 42 (nullBits) = 238 bits = 119 non-status
    /// dibits` accumulated in the message assembler. The framer's
    /// `mStatusSymbolDibitCounter` starts at 21 immediately after
    /// `nidDetected()` and increments by 1 BEFORE the `== 36` check, so
    /// the first body dibit takes the counter to 22. Status drops occur
    /// at body positions {14, 50, 86} (counter hits 36 → drop → reset
    /// to 0 → another 35 non-status dibits → next drop). After body
    /// position 121 the assembler has accumulated 119 non-status dibits
    /// and is complete; the next dibit would be a 4th status drop but
    /// we never read it. So total raw on-air body length = 14 + 1 +
    /// 35 + 1 + 35 + 1 + 35 = 122 dibits, with 3 status dibits
    /// embedded.
    ///
    /// Multi-block TSBK2 / TSBK3 handling is a follow-up; for now we
    /// read one block at a time and let the next sync detect catch the
    /// start of any subsequent block.
    pub fn length_dibits(self) -> usize {
        match self {
            Self::Hdu => 324,   // 648 bits
            Self::Tdu => 0,     // no payload
            Self::Ldu1 => 792,  // 1584 bits (9 IMBE frames + LC)
            Self::Tsdu => 122,  // TSBK1: 119 data+null + 3 status, see above
            Self::Ldu2 => 792,  // 1584 bits
            Self::Pdu => 288,   // variable, minimum
            Self::TduLc => 168, // 336 bits (LC + parity)
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
