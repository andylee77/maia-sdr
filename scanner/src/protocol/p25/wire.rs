//! P25 Phase 1 on-air wire-format constants.
//!
//! Single source of truth for protocol magic numbers (DUID codes,
//! frame sync pattern, NID geometry, body status dibit positions,
//! algorithm-ID sentinels). Every site that used to hold a local
//! copy now imports from here — when a TIA-102 clarification forces
//! a change, there is ONE place to edit.
//!
//! References: TIA-102.BAAA sections 6 + 7, SDRTrunk
//! `P25P1DataUnitID.java` / `P25P1SyncDetector.java`.

// ===================================================================
// DUID — Data Unit ID (4-bit value inside the NID).
// ===================================================================

pub const DUID_HDU:    u8 = 0x0;
pub const DUID_TDU:    u8 = 0x3;
pub const DUID_LDU1:   u8 = 0x5;
pub const DUID_TSDU:   u8 = 0x7;
pub const DUID_LDU2:   u8 = 0xA;
pub const DUID_PDU:    u8 = 0xC;
pub const DUID_TDU_LC: u8 = 0xF;

// ===================================================================
// Frame sync. 48 bits = 24 dibits, packed MSB-first into a u64.
// Same constant SDRTrunk uses (`P25P1SyncDetector.SYNC_PATTERN`).
// ===================================================================

/// Frame sync word, dibits packed MSB-first into the low 48 bits.
pub const FRAME_SYNC_PATTERN: u64 = 0x5575_F5FF_77FF;

/// 48-bit mask for the sync window inside a sliding u64 register.
pub const FRAME_SYNC_MASK: u64 = 0xFFFF_FFFF_FFFF;

// ===================================================================
// NID — Network Identifier, follows the frame sync.
// ===================================================================

/// On-air length of the NID window in dibits (32 payload + 1 status).
pub const NID_TRANSMITTED_DIBITS: usize = 33;

/// Position within the 33-dibit NID window where the first in-NID
/// P25 status dibit lands. Callers must advance the cursor past this
/// index but must not fold the dibit value into the 64-bit BCH
/// codeword: folded in, clean NIDs miscorrect to another codeword.
pub const NID_STATUS_DIBIT_INDEX: usize = 11;

// ===================================================================
// Encryption — algorithm_id sentinel.
// ===================================================================

/// Algorithm-ID value that denotes an UNENCRYPTED call per
/// TIA-102.AABD. Any other byte is an encrypted call (but see
/// `voice_frame::is_spec_algorithm_id` before trusting it — the
/// RS-decoded field can return bit-corrupt garbage).
pub const ALGORITHM_CLEAR: u8 = 0x80;
