//! Link control: ports of SDRTrunk `data/lc/LCMessageFactory`, the full LC
//! classes Tier III voice uses (`lc/full/GroupVoiceChannelUser`,
//! `UnitToUnitVoiceChannelUser`, `TerminatorData`, `EncryptionParameters`,
//! `UnknownFullLCMessage`) and the short LC classes of `lc/shorty/`.
//!
//! Validity is residual 0. SDRTrunk's `LCMessageFactory.java:134` has
//! `valid = (residual != 0)`, which marks good LCs bad (and lets its mask
//! manager pass bad ones); that is not copied.

use std::fmt;

use super::bits::{field, get, hex, hex_field};
use super::crc_mask::DmrCrcMaskManager;
use super::types::*;
use crate::protocol::dmr::fec::{crc, rs_12_9};

/// The SDRTrunk class a full LC decodes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullLcKind {
    GroupVoiceChannelUser,
    UnitToUnitVoiceChannelUser,
    TerminatorData,
    EncryptionParameters,
    /// Every other opcode (talker alias, GPS and vendor LCs included).
    Unknown,
}

/// A full link control message: 77 bits (embedded, BPTC(128,77)) or 96 bits
/// (voice header / terminator / PI header, RS(12,9) or CRC-CCITT).
#[derive(Debug, Clone)]
pub struct FullLc {
    pub bits: Vec<u8>,
    pub timeslot: u8,
    pub timestamp_ms: u64,
    pub valid: bool,
    /// SDRTrunk's corrected bit count (-1 / -2 when uncorrectable).
    pub corrected: i32,
    pub kind: FullLcKind,
}

/// Full LC opcode. Ports `FullLCMessage.getOpcode(message)`.
pub fn full_opcode(bits: &[u8]) -> LcOpcode {
    LcOpcode::from_value(
        true,
        field(bits, 2, 8),
        Vendor::from_value(field(bits, 8, 16)),
    )
}

/// Checks a full LC (5-bit checksum for 77 bits, RS(12,9) with the header or
/// terminator mask for 96) and builds its class. Ports `LCMessageFactory.createFull()`.
pub fn create_full(
    mut bits: Vec<u8>,
    timestamp_ms: u64,
    timeslot: u8,
    is_terminator: bool,
    corrected: i32,
    masks: &mut DmrCrcMaskManager,
) -> FullLc {
    let mut corrected = corrected;
    let mut residual = if bits.len() == 77 {
        crc::checksum_5(&bits)
    } else {
        let mask = if is_terminator {
            rs_12_9::TERMINATOR_LINK_CONTROL_CRC_MASK
        } else {
            rs_12_9::VOICE_LINK_CONTROL_CRC_MASK
        };
        rs_correct(&mut bits, mask, &mut corrected)
    };

    let opcode = full_opcode(&bits);

    // Some Hytera Tier III systems use a zero mask on the header / terminator LC.
    if residual != 0
        && bits.len() == 96
        && opcode == LcOpcode::FULL_STANDARD_GROUP_VOICE_CHANNEL_USER
    {
        residual = rs_correct(&mut bits, 0, &mut corrected);
    }

    let mut valid = residual == 0;
    if !valid {
        // An alternate mask (RAS) seen repeatedly for this opcode is accepted.
        valid = if bits.len() == 96 {
            masks.is_valid_rs12_9(opcode.value(), residual, timestamp_ms)
        } else {
            masks.is_valid_crc5(opcode.value(), residual, timestamp_ms)
        };
    }

    let kind = match opcode {
        LcOpcode::FULL_STANDARD_GROUP_VOICE_CHANNEL_USER => FullLcKind::GroupVoiceChannelUser,
        LcOpcode::FULL_STANDARD_UNIT_TO_UNIT_VOICE_CHANNEL_USER => {
            FullLcKind::UnitToUnitVoiceChannelUser
        }
        LcOpcode::FULL_STANDARD_TERMINATOR_DATA => FullLcKind::TerminatorData,
        LcOpcode::FULL_ENCRYPTION_PARAMETERS => {
            let mut lc = create_full_encryption(bits, timestamp_ms, timeslot);
            lc.corrected = corrected;
            return lc;
        }
        _ => FullLcKind::Unknown,
    };
    FullLc {
        bits,
        timeslot,
        timestamp_ms,
        valid,
        corrected,
        kind,
    }
}

/// RS(12,9) with `mask`: the residual (0 = good) and SDRTrunk's corrected count.
fn rs_correct(bits: &mut [u8], mask: u8, corrected: &mut i32) -> u32 {
    match rs_12_9::correct(bits, mask) {
        Ok(n) => {
            *corrected = n as i32;
            0
        }
        Err(residual) => {
            *corrected = -1;
            residual
        }
    }
}

/// The PI header's encryption parameters, CRC-CCITT protected. Ports
/// `LCMessageFactory.createFullEncryption()` / the `EncryptionParameters` constructor.
pub fn create_full_encryption(mut bits: Vec<u8>, timestamp_ms: u64, timeslot: u8) -> FullLc {
    let valid = crc::correct_ccitt80(&mut bits, crc::PI_HEADER_CRC_MASK).is_some();
    FullLc {
        bits,
        timeslot,
        timestamp_ms,
        valid,
        corrected: 0,
        kind: FullLcKind::EncryptionParameters,
    }
}

impl FullLc {
    /// SDRTrunk's simple class name.
    pub fn class_name(&self) -> &'static str {
        match self.kind {
            FullLcKind::GroupVoiceChannelUser => "GroupVoiceChannelUser",
            FullLcKind::UnitToUnitVoiceChannelUser => "UnitToUnitVoiceChannelUser",
            FullLcKind::TerminatorData => "TerminatorData",
            FullLcKind::EncryptionParameters => "EncryptionParameters",
            FullLcKind::Unknown => "UnknownFullLCMessage",
        }
    }

    pub fn opcode(&self) -> LcOpcode {
        if self.kind == FullLcKind::EncryptionParameters {
            return LcOpcode::FULL_ENCRYPTION_PARAMETERS;
        }
        full_opcode(&self.bits)
    }

    pub fn vendor(&self) -> Vendor {
        Vendor::from_value(field(&self.bits, 8, 16))
    }

    /// Voice call service options (group and unit-to-unit voice channel users).
    pub fn service_options(&self) -> Option<ServiceOptions> {
        match self.kind {
            FullLcKind::GroupVoiceChannelUser | FullLcKind::UnitToUnitVoiceChannelUser => {
                Some(ServiceOptions(field(&self.bits, 16, 24)))
            }
            _ => None,
        }
    }

    /// Talking radio of a voice channel user LC.
    pub fn source(&self) -> Option<Address> {
        match self.kind {
            FullLcKind::GroupVoiceChannelUser | FullLcKind::UnitToUnitVoiceChannelUser => {
                Some(Address::Radio(field(&self.bits, 48, 72)))
            }
            FullLcKind::TerminatorData => Some(Address::Radio(field(&self.bits, 40, 64))),
            _ => None,
        }
    }

    /// Talkgroup or radio called by a voice channel user LC.
    pub fn destination(&self) -> Option<Address> {
        let bits = &self.bits;
        match self.kind {
            FullLcKind::GroupVoiceChannelUser => Some(Address::Talkgroup(field(bits, 24, 48))),
            FullLcKind::UnitToUnitVoiceChannelUser => Some(Address::Radio(field(bits, 24, 48))),
            FullLcKind::TerminatorData if get(bits, 64) => {
                Some(Address::Talkgroup(field(bits, 16, 40)))
            }
            FullLcKind::TerminatorData => Some(Address::Radio(field(bits, 16, 40))),
            _ => None,
        }
    }
}

impl fmt::Display for FullLc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bits = &self.bits;
        match self.kind {
            FullLcKind::GroupVoiceChannelUser | FullLcKind::UnitToUnitVoiceChannelUser => {
                if !self.valid {
                    f.write_str("[CRC-ERROR] ")?;
                }
                let name = if self.kind == FullLcKind::GroupVoiceChannelUser {
                    "FLC GROUP VOICE CHANNEL USER"
                } else {
                    "FLC UNIT TO UNIT VOICE CHANNEL USER"
                };
                write!(
                    f,
                    "{name} FM:{} TO:{}",
                    self.source().unwrap(),
                    self.destination().unwrap()
                )?;
                write!(f, " {}", self.service_options().unwrap())
            }
            FullLcKind::TerminatorData => {
                if !self.valid {
                    write!(f, "[CRC ERROR {}] ", self.corrected)?;
                }
                write!(
                    f,
                    "FM:{} TO:{}",
                    self.source().unwrap(),
                    self.destination().unwrap()
                )?;
                f.write_str(if get(bits, 66) {
                    " COMPLETE"
                } else {
                    " FRAGMENT"
                })?;
                write!(f, " SEQUENCE:{}", field(bits, 69, 72))?;
                if get(bits, 68) {
                    f.write_str(" RESYNC")?;
                }
                if get(bits, 65) {
                    f.write_str(" ACK:YES")?;
                }
                Ok(())
            }
            FullLcKind::EncryptionParameters => {
                if !self.valid {
                    f.write_str("[CRC-ERROR] ")?;
                }
                if get(bits, 0) {
                    f.write_str(" *ENCRYPTED*")?;
                }
                if get(bits, 1) {
                    f.write_str(" *RESERVED-BIT*")?;
                }
                write!(f, "FLC ENCRYPTION PARAMETERS VENDOR:{}", self.vendor())?;
                let algorithm = field(bits, 2, 8);
                match encryption_algorithm(algorithm) {
                    Some(label) => write!(f, " ALGORITHM:{label}")?,
                    None => write!(f, " ALGORITHM:{algorithm}")?,
                }
                write!(f, " KEY:{}", field(bits, 16, 24))?;
                write!(f, " IV:{}", hex_field(bits, 24, 56, 8))?;
                write!(f, " TALKGROUP:{}", field(bits, 56, 80))?;
                write!(f, " MSG:{}", hex(bits))
            }
            FullLcKind::Unknown => {
                if !self.valid {
                    f.write_str("[CRC-ERROR] ")?;
                }
                f.write_str("FLC UNKNOWN")?;
                match full_opcode(bits) {
                    LcOpcode::FULL_STANDARD_UNKNOWN => write!(f, " OPCODE:{}", field(bits, 2, 8))?,
                    opcode => write!(f, " {opcode}")?,
                }
                match self.vendor() {
                    Vendor::Standard => {}
                    Vendor::Unknown => write!(f, " VENDOR:{}", field(bits, 8, 16))?,
                    vendor => write!(f, " VENDOR:{vendor}")?,
                }
                write!(f, " MSG:{}", hex(bits))
            }
        }
    }
}

/// The SDRTrunk class a short LC decodes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortLcKind {
    Null,
    ActivityUpdate,
    ControlChannelSystemParameters,
    TrafficChannelSystemParameters,
    CapacityPlusRestChannel,
    ConnectPlusControlChannel,
    ConnectPlusTrafficChannel,
    HyteraXptChannel,
    Unknown,
}

/// A short link control message from the CACH: 36 bits (4 fragments, 28 + CRC-8)
/// or 17 (a single fragment).
#[derive(Debug, Clone)]
pub struct ShortLc {
    pub bits: Vec<u8>,
    pub timestamp_ms: u64,
    pub valid: bool,
    pub kind: ShortLcKind,
}

/// Short LC vendor: bit 0 doubles as the vendor flag. Ports `ShortLCMessage.getVendor()`.
pub fn short_vendor(bits: &[u8]) -> Vendor {
    if get(bits, 0) && field(bits, 0, 4) != 10 {
        match Vendor::from_value(field(bits, 4, 12)) {
            Vendor::Unknown => Vendor::Standard,
            vendor => vendor,
        }
    } else {
        Vendor::Standard
    }
}

/// Builds the short LC class; valid when the CRC-8 over 36 bits (17-bit
/// fragments padded with zeros, as SDRTrunk does) is 0. Ports `LCMessageFactory.createShort()`.
pub fn create_short(bits: Vec<u8>, timestamp_ms: u64) -> ShortLc {
    let opcode = LcOpcode::from_value(false, field(&bits, 0, 4), short_vendor(&bits));
    let kind = match opcode {
        LcOpcode::SHORT_STANDARD_NULL_MESSAGE => ShortLcKind::Null,
        LcOpcode::SHORT_STANDARD_ACTIVITY_UPDATE => ShortLcKind::ActivityUpdate,
        LcOpcode::SHORT_CAPACITY_PLUS_REST_CHANNEL_NOTIFICATION => {
            ShortLcKind::CapacityPlusRestChannel
        }
        LcOpcode::SHORT_CONNECT_PLUS_CONTROL_CHANNEL => ShortLcKind::ConnectPlusControlChannel,
        LcOpcode::SHORT_CONNECT_PLUS_TRAFFIC_CHANNEL => ShortLcKind::ConnectPlusTrafficChannel,
        LcOpcode::SHORT_STANDARD_CONTROL_CHANNEL_SYSTEM_PARAMETERS => {
            ShortLcKind::ControlChannelSystemParameters
        }
        LcOpcode::SHORT_STANDARD_TRAFFIC_CHANNEL_SYSTEM_PARAMETERS => {
            ShortLcKind::TrafficChannelSystemParameters
        }
        LcOpcode::SHORT_HYTERA_XPT_CHANNEL | LcOpcode::SHORT_STANDARD_XPT_CHANNEL => {
            ShortLcKind::HyteraXptChannel
        }
        _ => ShortLcKind::Unknown,
    };
    let mut padded = bits.clone();
    padded.resize(36, 0);
    let valid = crc::crc8(&padded) == 0;
    ShortLc {
        bits,
        timestamp_ms,
        valid,
        kind,
    }
}

impl ShortLc {
    /// SDRTrunk's simple class name.
    pub fn class_name(&self) -> &'static str {
        match self.kind {
            ShortLcKind::Null => "NullMessage",
            ShortLcKind::ActivityUpdate => "ActivityUpdateMessage",
            ShortLcKind::ControlChannelSystemParameters => "ControlChannelSystemParameters",
            ShortLcKind::TrafficChannelSystemParameters => "TrafficChannelSystemParameters",
            ShortLcKind::CapacityPlusRestChannel => "CapacityPlusRestChannel",
            ShortLcKind::ConnectPlusControlChannel => "ConnectPlusControlChannel",
            ShortLcKind::ConnectPlusTrafficChannel => "ConnectPlusTrafficChannel",
            ShortLcKind::HyteraXptChannel => "HyteraXPTChannel",
            ShortLcKind::Unknown => "UnknownShortLCMessage",
        }
    }

    /// System identity code of the Tier III control / traffic channel parameters.
    pub fn system_identity_code(&self) -> Option<SystemIdentityCode> {
        match self.kind {
            ShortLcKind::ControlChannelSystemParameters
            | ShortLcKind::TrafficChannelSystemParameters => {
                Some(SystemIdentityCode::new(&self.bits, 4, false))
            }
            _ => None,
        }
    }
}

impl fmt::Display for ShortLc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bits = &self.bits;
        if !self.valid {
            f.write_str("[CRC ERROR] ")?;
        }
        match self.kind {
            ShortLcKind::Null => f.write_str("SLC TS1:IDLE TS2:IDLE"),
            ShortLcKind::ActivityUpdate => {
                let (ts1, ts2) = (field(bits, 4, 8), field(bits, 8, 12));
                write!(f, "SLC TS1:{}", activity_label(ts1))?;
                if ts1 != 0 {
                    write!(f, " [{}]", hex_field(bits, 12, 20, 2))?;
                }
                write!(f, " TS2:{}", activity_label(ts2))?;
                if ts2 != 0 {
                    write!(f, " [{}]", hex_field(bits, 20, 28, 2))?;
                }
                write!(f, " MSG:{}", hex(bits))
            }
            ShortLcKind::ControlChannelSystemParameters
            | ShortLcKind::TrafficChannelSystemParameters => {
                let sic = self.system_identity_code().unwrap();
                let channel = if self.kind == ShortLcKind::ControlChannelSystemParameters {
                    "CONTROL"
                } else {
                    "TRAFFIC"
                };
                write!(f, "SLC TIER III {channel} CHANNEL {}", sic.model_label())?;
                write!(f, " NET:{} SITE:{}", sic.network, sic.site)?;
                if get(bits, 18) {
                    f.write_str(" REGISTRATION REQUIRED")?;
                }
                write!(f, " SLOT COUNTER:{}", field(bits, 19, 28))?;
                write!(f, " MSG:{}", hex(bits))
            }
            ShortLcKind::CapacityPlusRestChannel => {
                write!(
                    f,
                    "SLC MOTOROLA CAP+ SITE:{} REST:{}",
                    field(bits, 20, 25),
                    field(bits, 15, 20)
                )?;
                write!(f, " MSG:{}", hex(bits))
            }
            ShortLcKind::ConnectPlusControlChannel | ShortLcKind::ConnectPlusTrafficChannel => {
                let channel = if self.kind == ShortLcKind::ConnectPlusControlChannel {
                    "CONTROL"
                } else {
                    "TRAFFIC"
                };
                write!(
                    f,
                    "SLC MOTOROLA CON+ {channel} CHANNEL NETWORK:{}",
                    field(bits, 4, 16)
                )?;
                write!(f, " SITE:{} MSG:{}", field(bits, 16, 24), hex(bits))
            }
            ShortLcKind::HyteraXptChannel => {
                f.write_str("SLC HYTERA XPT")?;
                let free = field(bits, 12, 16);
                if free == 0 {
                    f.write_str(" ALL REPEATERS BUSY")?;
                } else {
                    write!(f, " FREE REPEATER:{free}")?;
                }
                let priority = field(bits, 16, 20);
                if priority > 0 {
                    write!(
                        f,
                        " PRIORITY CALL FOR:{} ON REPEATER:{priority}",
                        hex_field(bits, 20, 28, 2)
                    )?;
                }
                write!(f, " MSG:{}", hex(bits))
            }
            ShortLcKind::Unknown => write!(
                f,
                "SLC UNKNOWN OPCODE:{} MSG:{}",
                field(bits, 0, 4),
                hex(bits)
            ),
        }
    }
}

#[cfg(test)]
#[path = "lc_tests.rs"]
mod tests;
