//! Control signalling blocks: ports of SDRTrunk `data/csbk/CSBKMessage`,
//! `CSBKMessageFactory`, `UnknownCSBKMessage`, all of `data/csbk/standard/*`,
//! `data/header/MBCHeader` and `data/mbc/UnknownMultiCSBK`.
//!
//! Fields are read from the 96 payload bits on demand, as the Java getters do;
//! `Display` is each class's `toString()`.

use std::fmt;

use super::bits::{field, get, hex, hex_field};
use super::data::DataBurst;
use super::types::*;
use crate::protocol::dmr::fec::crc;
use crate::protocol::dmr::fec::slot_type::DataType;

/// The SDRTrunk class a CSBK decodes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsbkKind {
    Aloha,
    Clear,
    MoveTscc,
    Preamble,
    Protect,
    Acknowledge,
    AcknowledgeStatus,
    RegistrationAccepted,
    AuthenticateRegisterRadioCheck,
    CancelCall,
    ServiceRadioCheck,
    StunReviveKill,
    UnknownAhoy,
    Announcement,
    AdjacentSiteInformation,
    AnnounceChannelFrequency,
    AnnounceWithdrawTscc,
    CallTimerParameters,
    LocalTime,
    MassRegistration,
    VoteNowAdvice,
    BroadcastTalkgroupVoiceChannelGrant,
    TalkgroupVoiceChannelGrant,
    PrivateVoiceChannelGrant,
    DuplexPrivateVoiceChannelGrant,
    PrivateDataChannelGrant,
    DuplexPrivateDataChannelGrant,
    TalkgroupDataChannelGrant,
    /// Any other opcode, vendor CSBKs included (`UnknownCSBKMessage`).
    Unknown,
    /// A multi-block CSBK with an opcode SDRTrunk does not assemble (`UnknownMultiCSBK`).
    UnknownMulti,
    /// The header block of a multi-block CSBK (`MBCHeader`).
    MbcHeader,
}

impl CsbkKind {
    /// SDRTrunk's simple class name.
    pub fn class_name(self) -> &'static str {
        use CsbkKind::*;
        match self {
            Aloha => "Aloha",
            Clear => "Clear",
            MoveTscc => "MoveTSCC",
            Preamble => "Preamble",
            Protect => "Protect",
            Acknowledge => "Acknowledge",
            AcknowledgeStatus => "AcknowledgeStatus",
            RegistrationAccepted => "RegistrationAccepted",
            AuthenticateRegisterRadioCheck => "AuthenticateRegisterRadioCheck",
            CancelCall => "CancelCall",
            ServiceRadioCheck => "ServiceRadioCheck",
            StunReviveKill => "StunReviveKill",
            UnknownAhoy => "UnknownAhoy",
            Announcement => "Announcement",
            AdjacentSiteInformation => "AdjacentSiteInformation",
            AnnounceChannelFrequency => "AnnounceChannelFrequency",
            AnnounceWithdrawTscc => "AnnounceWithdrawTSCC",
            CallTimerParameters => "CallTimerParameters",
            LocalTime => "LocalTime",
            MassRegistration => "MassRegistration",
            VoteNowAdvice => "VoteNowAdvice",
            BroadcastTalkgroupVoiceChannelGrant => "BroadcastTalkgroupVoiceChannelGrant",
            TalkgroupVoiceChannelGrant => "TalkgroupVoiceChannelGrant",
            PrivateVoiceChannelGrant => "PrivateVoiceChannelGrant",
            DuplexPrivateVoiceChannelGrant => "DuplexPrivateVoiceChannelGrant",
            PrivateDataChannelGrant => "PrivateDataChannelGrant",
            DuplexPrivateDataChannelGrant => "DuplexPrivateDataChannelGrant",
            TalkgroupDataChannelGrant => "TalkgroupDataChannelGrant",
            Unknown => "UnknownCSBKMessage",
            UnknownMulti => "UnknownMultiCSBK",
            MbcHeader => "MBCHeader",
        }
    }

    /// Channel grants (SDRTrunk `ChannelGrant` subclasses).
    pub fn is_channel_grant(self) -> bool {
        use CsbkKind::*;
        matches!(
            self,
            BroadcastTalkgroupVoiceChannelGrant
                | TalkgroupVoiceChannelGrant
                | PrivateVoiceChannelGrant
                | DuplexPrivateVoiceChannelGrant
                | PrivateDataChannelGrant
                | DuplexPrivateDataChannelGrant
                | TalkgroupDataChannelGrant
        )
    }

    /// Kinds SDRTrunk enriches with LCN frequencies (`ITimeslotFrequencyReceiver`).
    pub fn takes_frequencies(self) -> bool {
        self.is_channel_grant() || matches!(self, CsbkKind::Clear | CsbkKind::VoteNowAdvice)
    }
}

/// A CSBK (or multi-block CSBK) with its payload and the channel it names.
#[derive(Debug, Clone)]
pub struct Csbk {
    pub burst: DataBurst,
    pub kind: CsbkKind,
    /// Continuation blocks of a multi-block CSBK (96 bits each).
    pub blocks: Vec<Vec<u8>>,
    /// Absolute channel parameters from continuation block 1.
    pub absolute: Option<AbsoluteChannelParameters>,
    /// The channel a grant, clear, vote-now, move or neighbour names; enriched
    /// with frequencies from the LCN map when SDRTrunk would.
    pub channel: Option<DmrChannel>,
}

const PROTECT_FLAG: usize = 1;
const SOURCE: (usize, usize) = (56, 80);
const DESTINATION: (usize, usize) = (32, 56);

/// CSBK opcode of a payload. Ports `CSBKMessage.getOpcode(message)`.
pub fn opcode(bits: &[u8]) -> Opcode {
    Opcode::from_value(field(bits, 2, 8), vendor(bits))
}

/// Ports `CSBKMessage.getVendor(message)`.
pub fn vendor(bits: &[u8]) -> Vendor {
    Vendor::from_value(field(bits, 8, 16))
}

/// Builds the CSBK class for a single block and checks its CRC. Ports `CSBKMessageFactory.create(...)`.
pub fn create(burst: DataBurst) -> Csbk {
    use CsbkKind::*;
    let bits = &burst.bits;
    let kind = match opcode(bits) {
        Opcode::StandardAcknowledgeResponseInboundTscc
        | Opcode::StandardAcknowledgeResponseOutboundTscc
        | Opcode::StandardAcknowledgeResponseInboundPayload
        | Opcode::StandardAcknowledgeResponseOutboundPayload => match field(bits, 23, 31) {
            0x62 => RegistrationAccepted,
            0x63 => AcknowledgeStatus,
            _ => Acknowledge,
        },
        Opcode::StandardAhoy => match field(bits, 28, 32) {
            14 => AuthenticateRegisterRadioCheck,
            15 => CancelCall,
            13 => StunReviveKill,
            0..=5 | 10 | 11 => ServiceRadioCheck,
            _ => UnknownAhoy,
        },
        Opcode::StandardAloha => Aloha,
        Opcode::StandardAnnouncement => match field(bits, 16, 21) {
            6 => AdjacentSiteInformation,
            0 => AnnounceWithdrawTscc,
            1 => CallTimerParameters,
            3 => LocalTime,
            4 => MassRegistration,
            2 => VoteNowAdvice,
            _ => Announcement,
        },
        Opcode::StandardBroadcastTalkgroupVoiceChannelGrant => BroadcastTalkgroupVoiceChannelGrant,
        Opcode::StandardClear => Clear,
        Opcode::StandardDuplexPrivateDataChannelGrant => DuplexPrivateDataChannelGrant,
        Opcode::StandardDuplexPrivateVoiceChannelGrant => DuplexPrivateVoiceChannelGrant,
        Opcode::StandardPrivateDataChannelGrantSingleItem => PrivateDataChannelGrant,
        Opcode::StandardPrivateVoiceChannelGrant => PrivateVoiceChannelGrant,
        Opcode::StandardProtect => Protect,
        Opcode::StandardTalkgroupDataChannelGrantSingleItem => TalkgroupDataChannelGrant,
        Opcode::StandardTalkgroupVoiceChannelGrant => TalkgroupVoiceChannelGrant,
        Opcode::StandardMoveTscc => MoveTscc,
        Opcode::StandardPreamble => Preamble,
        _ => Unknown,
    };
    let mut csbk = Csbk {
        burst,
        kind,
        blocks: Vec::new(),
        absolute: None,
        channel: None,
    };
    csbk.channel = csbk.compute_channel();
    csbk.check_crc();
    csbk
}

/// Builds the CSBK class for a multi-block CSBK from its header and
/// continuation blocks and checks its CRC. Ports `CSBKMessageFactory.create(header, blocks)`.
pub fn create_multi(header: &Csbk, blocks: Vec<Vec<u8>>) -> Csbk {
    use CsbkKind::*;
    let bits = &header.burst.bits;
    let kind = match opcode(bits) {
        Opcode::StandardAnnouncement => match field(bits, 16, 21) {
            6 => AdjacentSiteInformation,
            0 => AnnounceWithdrawTscc,
            5 => AnnounceChannelFrequency,
            2 => VoteNowAdvice,
            _ => UnknownMulti,
        },
        Opcode::StandardBroadcastTalkgroupVoiceChannelGrant => BroadcastTalkgroupVoiceChannelGrant,
        Opcode::StandardClear => Clear,
        Opcode::StandardDuplexPrivateDataChannelGrant => DuplexPrivateDataChannelGrant,
        Opcode::StandardDuplexPrivateVoiceChannelGrant => DuplexPrivateVoiceChannelGrant,
        Opcode::StandardMoveTscc => MoveTscc,
        Opcode::StandardPrivateDataChannelGrantSingleItem => PrivateDataChannelGrant,
        Opcode::StandardPrivateVoiceChannelGrant => PrivateVoiceChannelGrant,
        Opcode::StandardTalkgroupDataChannelGrantSingleItem => TalkgroupDataChannelGrant,
        Opcode::StandardTalkgroupVoiceChannelGrant => TalkgroupVoiceChannelGrant,
        _ => UnknownMulti,
    };
    let mut burst = header.burst.clone();
    burst.valid = true;
    let absolute = match (kind, blocks.first()) {
        (UnknownMulti, _) | (_, None) => None,
        (_, Some(block)) => {
            // Grants move to their own timeslot; the rest describe timeslot 1.
            let timeslot = if kind.is_channel_grant() {
                field(bits, 28, 29) as u8 + 1
            } else {
                1
            };
            Some(AbsoluteChannelParameters {
                bits: block.clone(),
                timeslot,
            })
        }
    };
    let mut csbk = Csbk {
        burst,
        kind,
        blocks,
        absolute,
        channel: None,
    };
    csbk.channel = csbk.compute_channel();
    csbk.check_crc();
    csbk
}

impl Csbk {
    pub fn class_name(&self) -> &'static str {
        self.kind.class_name()
    }

    pub fn bits(&self) -> &[u8] {
        &self.burst.bits
    }

    pub fn opcode(&self) -> Opcode {
        opcode(self.bits())
    }

    pub fn vendor(&self) -> Vendor {
        vendor(self.bits())
    }

    /// Ports `checkCRC()`, with the multi-block override of the classes that have one.
    fn check_crc(&mut self) {
        use CsbkKind::*;
        let multi_block_capable = matches!(
            self.kind,
            Clear
                | VoteNowAdvice
                | AdjacentSiteInformation
                | AnnounceWithdrawTscc
                | AnnounceChannelFrequency
                | MoveTscc
        ) || self.kind.is_channel_grant();
        if self.kind == MbcHeader {
            // MBCHeader has no CRC check of its own; the BPTC result stands.
            return;
        }
        if multi_block_capable && self.burst.data_type == DataType::MbcHeader {
            self.check_multi_block_crc();
        } else {
            self.burst.valid =
                crc::correct_ccitt80(&mut self.burst.bits, crc::CSBK_CRC_MASK).is_some();
        }
    }

    /// Header with the MBC header mask, block 1 with the last-block mask.
    /// Ports `checkMultiBlockCRC()`; SDRTrunk's last-block mask 0x0000 also
    /// accepts 0xFFFF, so both are tried.
    fn check_multi_block_crc(&mut self) {
        let header = crc::correct_ccitt80(&mut self.burst.bits, crc::MBC_HEADER_CRC_MASK).is_some();
        let block = match &mut self.absolute {
            None => true,
            Some(parameters) => {
                crc::correct_ccitt80(&mut parameters.bits, crc::MBC_LAST_BLOCK_CRC_MASK).is_some()
                    || crc::correct_ccitt80(&mut parameters.bits, 0xFFFF).is_some()
            }
        };
        self.burst.valid = header && block;
    }

    /// The channel this CSBK names, if any.
    fn compute_channel(&self) -> Option<DmrChannel> {
        use CsbkKind::*;
        if let Some(parameters) = &self.absolute {
            return Some(parameters.channel());
        }
        let bits = self.bits();
        match self.kind {
            k if k.is_channel_grant() => Some(DmrChannel::tier3(
                field(bits, 16, 28),
                field(bits, 28, 29) as u8 + 1,
            )),
            Clear => Some(DmrChannel::tier3(field(bits, 16, 28), 1)),
            VoteNowAdvice | AdjacentSiteInformation => {
                Some(DmrChannel::tier3(field(bits, 68, 80), 1))
            }
            MoveTscc => Some(DmrChannel::tier3(field(bits, 44, 56), 1)),
            AnnounceWithdrawTscc => Some(DmrChannel::tier3(field(bits, 56, 68), 1)),
            _ => None,
        }
    }

    /// Ports `isEncrypted()` (the protect flag).
    pub fn is_encrypted(&self) -> bool {
        get(self.bits(), PROTECT_FLAG)
    }

    fn is_emergency(&self) -> bool {
        get(self.bits(), 30)
    }

    /// Source of a grant, clear, protect, preamble or acknowledge.
    pub fn source(&self) -> Option<Address> {
        use CsbkKind::*;
        let bits = self.bits();
        let source = field(bits, SOURCE.0, SOURCE.1);
        let destination = field(bits, DESTINATION.0, DESTINATION.1);
        Some(match self.kind {
            BroadcastTalkgroupVoiceChannelGrant
            | TalkgroupVoiceChannelGrant
            | TalkgroupDataChannelGrant => Address::Radio(source),
            DuplexPrivateVoiceChannelGrant | DuplexPrivateDataChannelGrant if get(bits, 31) => {
                Address::Tier3Radio(destination)
            }
            PrivateVoiceChannelGrant
            | DuplexPrivateVoiceChannelGrant
            | PrivateDataChannelGrant
            | DuplexPrivateDataChannelGrant
            | Clear
            | Protect
            | Preamble
            | Acknowledge
            | AcknowledgeStatus
            | RegistrationAccepted => Address::Tier3Radio(source),
            AuthenticateRegisterRadioCheck
            | CancelCall
            | ServiceRadioCheck
            | StunReviveKill
            | UnknownAhoy => Address::Radio(source),
            _ => return None,
        })
    }

    /// Destination of a grant, clear, protect, preamble, acknowledge or ahoy.
    pub fn destination(&self) -> Option<Address> {
        use CsbkKind::*;
        let bits = self.bits();
        let source = field(bits, SOURCE.0, SOURCE.1);
        let destination = field(bits, DESTINATION.0, DESTINATION.1);
        let by_flag = |flag: usize| {
            if get(bits, flag) {
                Address::Talkgroup(destination)
            } else {
                Address::Tier3Radio(destination)
            }
        };
        Some(match self.kind {
            BroadcastTalkgroupVoiceChannelGrant
            | TalkgroupVoiceChannelGrant
            | TalkgroupDataChannelGrant => Address::Talkgroup(destination),
            DuplexPrivateVoiceChannelGrant | DuplexPrivateDataChannelGrant if get(bits, 31) => {
                Address::Tier3Radio(source)
            }
            PrivateVoiceChannelGrant
            | DuplexPrivateVoiceChannelGrant
            | PrivateDataChannelGrant
            | DuplexPrivateDataChannelGrant => Address::Tier3Radio(destination),
            Clear | Protect => by_flag(31),
            Preamble => by_flag(17),
            Acknowledge | AcknowledgeStatus | RegistrationAccepted => by_flag(16),
            AuthenticateRegisterRadioCheck
            | CancelCall
            | ServiceRadioCheck
            | StunReviveKill
            | UnknownAhoy => by_flag(25),
            Aloha | MoveTscc | MassRegistration => Address::Tier3Radio(field(bits, 56, 80)),
            _ => return None,
        })
    }

    /// The system identity code at bit 40 (ALOHA and announcements).
    pub fn system_identity_code(&self) -> SystemIdentityCode {
        SystemIdentityCode::new(self.bits(), 40, true)
    }

    fn write_prefix(&self, f: &mut fmt::Formatter<'_>, ras: bool) -> fmt::Result {
        if !self.burst.valid {
            f.write_str("[CRC-ERROR] ")?;
        }
        write!(f, "CC:{}", self.burst.color_code)?;
        if ras && self.burst.has_ras() {
            write!(f, " RAS:{}", self.burst.reserved)?;
        }
        Ok(())
    }

    fn write_this_site(&self, f: &mut fmt::Formatter<'_>, sic: &SystemIdentityCode) -> fmt::Result {
        write!(
            f,
            " {} NETWORK:{} SITE:{}",
            sic.model_label(),
            sic.network,
            sic.site
        )
    }

    fn write_unknown(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_prefix(f, true)?;
        f.write_str(" CSBK *UNKNOWN*")?;
        match self.vendor() {
            Vendor::Unknown => write!(f, " VENDOR:{}", field(self.bits(), 8, 16))?,
            Vendor::Standard => {}
            vendor => write!(f, " {vendor}")?,
        }
        match self.opcode() {
            Opcode::Unknown => write!(f, " UNKNOWN OPCODE:{}", field(self.bits(), 2, 8))?,
            opcode => write!(f, " {opcode}")?,
        }
        write!(f, " MSG:{}", hex(self.bits()))?;
        if self.kind == CsbkKind::UnknownMulti {
            for block in &self.blocks {
                f.write_str(&hex(block))?;
            }
        }
        Ok(())
    }

    fn write_grant(
        &self,
        f: &mut fmt::Formatter<'_>,
        name: &str,
        hi_rate: bool,
        msg: bool,
    ) -> fmt::Result {
        self.write_prefix(f, true)?;
        if self.is_emergency() {
            f.write_str(" EMERGENCY")?;
        }
        if self.is_encrypted() {
            f.write_str(" ENCRYPTED")?;
        }
        if hi_rate && get(self.bits(), 31) {
            f.write_str(" DUAL-SLOT HI-RATE ")?;
        }
        write!(
            f,
            " {name} FM:{} TO:{}",
            self.source().unwrap(),
            self.destination().unwrap()
        )?;
        write!(f, " {}", self.channel.unwrap())?;
        if msg {
            write!(f, " MSG:{}", hex(self.bits()))?;
        }
        Ok(())
    }
}

/// `CallTimerParameters` emergency timer text.
fn emergency_timer(timer: u32) -> String {
    match timer {
        0 => "INTERNAL".into(),
        1..=10 => format!("{timer} SECS"),
        11..=20 => format!("{} SECS", (timer - 8) * 5),
        21..=28 => format!("{} SECS", (timer - 16) * 15),
        29..=40 => format!("{} MINS", java_double(f64::from(timer - 22) * 0.5)),
        41..=51 => format!("{} MINS", timer - 31),
        52..=510 => format!("{} MINS", (timer - 47) * 5),
        _ => "INFINITY".into(),
    }
}

/// `CallTimerParameters` packet timer text.
fn packet_timer(timer: u32) -> String {
    match timer {
        0 => "INTERNAL".into(),
        1..=5 => format!("{timer} SECS"),
        6..=10 => format!("{} SECS", (timer - 4) * 5),
        11..=12 => format!("{} SECS", (timer - 8) * 15),
        13..=20 => format!("{} MINS", java_double(f64::from(timer - 10) * 0.5)),
        21..=25 => format!("{} MINS", timer - 15),
        26..=30 => format!("{} MINS", (timer - 23) * 5),
        _ => "INFINITY".into(),
    }
}

/// `CallTimerParameters` mobile-to-mobile and mobile-to-line timer text.
fn call_timer(timer: u32) -> String {
    match timer {
        0 => "INTERNAL".into(),
        1..=59 => format!("{timer} SECS"),
        60..=107 => format!("{} SECS", (timer - 48) * 5),
        108..=138 => format!("{} MINS", java_double(f64::from(timer - 98) * 0.5)),
        139..=4094 => format!("{} MINS", timer - 118),
        _ => "INFINITY".into(),
    }
}

impl fmt::Display for Csbk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use CsbkKind::*;
        let bits = self.bits();
        match self.kind {
            Aloha => {
                let sic = SystemIdentityCode::new(bits, 40, true);
                self.write_prefix(f, true)?;
                f.write_str(" ALOHA")?;
                if field(bits, 56, 80) != 0 {
                    write!(f, " TO:{}", self.destination().unwrap())?;
                }
                write!(
                    f,
                    " {} NETWORK:{} SITE:{}",
                    sic.model_label(),
                    sic.network,
                    sic.site
                )?;
                f.write_str(if get(bits, 23) {
                    " NET-CONNECTED"
                } else {
                    " NET-DISCONNECTED"
                })?;
                write!(
                    f,
                    " SERVICES:{}",
                    service_function_label(field(bits, 29, 31))
                )?;
                write!(f, " ETSI VER:{}", version_label(field(bits, 19, 22)))?;
                write!(f, " MASK:{}", field(bits, 24, 29))?;
                if sic.is_multiple_control_channels() {
                    write!(f, " {}", par_label(sic.par.unwrap_or(0)))?;
                }
                write!(f, " {}", hex(bits))
            }
            Clear => {
                self.write_prefix(f, true)?;
                if self.is_encrypted() {
                    f.write_str(" ENCRYPTED")?;
                }
                write!(f, " CLEAR - RETURN TO{}", self.channel.unwrap())?;
                write!(
                    f,
                    " FM:{} TO:{}",
                    self.source().unwrap(),
                    self.destination().unwrap()
                )?;
                write!(f, " MSG:{}", hex(bits))
            }
            Protect => {
                self.write_prefix(f, true)?;
                if self.is_encrypted() {
                    f.write_str(" ENCRYPTED")?;
                }
                write!(f, " PROTECT: {}", protect_kind_label(field(bits, 28, 31)))?;
                write!(
                    f,
                    " FM:{} TO:{}",
                    self.source().unwrap(),
                    self.destination().unwrap()
                )
            }
            MoveTscc => {
                self.write_prefix(f, true)?;
                write!(f, " MOVE TRUNK CONTROL CHANNEL {}", self.channel.unwrap())?;
                if field(bits, 56, 80) != 0 {
                    write!(f, " TO:{}", self.destination().unwrap())?;
                }
                Ok(())
            }
            Preamble => {
                self.write_prefix(f, true)?;
                write!(
                    f,
                    " CSBK PREAMBLE FM:{} TO:{}",
                    self.source().unwrap(),
                    self.destination().unwrap()
                )?;
                f.write_str(if get(bits, 16) { " DATA" } else { " CSBK" })?;
                write!(f, " BLOCKS TO FOLLOW:{}", field(bits, 24, 32))?;
                write!(f, " MSG:{}", hex(bits))
            }
            Acknowledge => {
                self.write_prefix(f, true)?;
                write!(f, " {}", acknowledge_type_label(field(bits, 23, 25)))?;
                write!(
                    f,
                    " REASON:{}",
                    reason(field(bits, 23, 31)).unwrap_or("UNKNOWN")
                )?;
                write!(
                    f,
                    " FM:{} TO:{}",
                    self.source().unwrap(),
                    self.destination().unwrap()
                )
            }
            AcknowledgeStatus => {
                self.write_prefix(f, false)?;
                write!(f, " ACKNOWLEDGE STATUS:{}", field(bits, 16, 23))?;
                write!(
                    f,
                    " TO:{} FM:{}",
                    self.destination().unwrap(),
                    self.source().unwrap()
                )
            }
            RegistrationAccepted => {
                self.write_prefix(f, false)?;
                write!(
                    f,
                    " REGISTRATION ACCEPTED TO:{}",
                    self.destination().unwrap()
                )?;
                write!(f, " FM:{}", self.source().unwrap())?;
                let offset = field(bits, 16, 23);
                if offset > 0 {
                    write!(f, " POWER SAVE OFFSET:{offset}")?;
                }
                Ok(())
            }
            AuthenticateRegisterRadioCheck => {
                self.write_prefix(f, true)?;
                if self.is_encrypted() {
                    f.write_str(" ENCRYPTED")?;
                }
                let radio_check = field(bits, 56, 80) == 0xFFFECA;
                let command = if radio_check {
                    "RADIO CHECK"
                } else {
                    "AUTHENTICATE"
                };
                write!(f, " {command}:{}", self.destination().unwrap())?;
                if !radio_check {
                    write!(f, " CHALLENGE VALUE:{}", hex_field(bits, 56, 80, 6))?;
                }
                Ok(())
            }
            CancelCall => {
                self.write_prefix(f, true)?;
                if self.is_encrypted() {
                    f.write_str(" ENCRYPTED")?;
                }
                write!(
                    f,
                    " CANCEL CALL TO:{} FM:{}",
                    self.destination().unwrap(),
                    self.source().unwrap()
                )
            }
            ServiceRadioCheck => {
                self.write_prefix(f, true)?;
                if self.is_encrypted() {
                    f.write_str(" ENCRYPTED")?;
                }
                let service = match field(bits, 28, 32) {
                    2 | 3 | 11 => "PACKET",
                    4 | 5 => "SHORT DATA",
                    0 | 1 | 10 => "VOICE",
                    _ => "UNKNOWN",
                };
                let target = if get(bits, 25) {
                    "TALKGROUP"
                } else {
                    "INDIVIDUAL"
                };
                write!(
                    f,
                    " {service} SERVICE RADIO CHECK ({target}) TO:{}",
                    self.destination().unwrap()
                )?;
                write!(f, " FM:{}", self.source().unwrap())
            }
            StunReviveKill => {
                self.write_prefix(f, true)?;
                if self.is_encrypted() {
                    f.write_str(" ENCRYPTED")?;
                }
                let command = match field(bits, 56, 80) {
                    0xFFFECF => "KILL",
                    0xFFFECC if !get(bits, 23) => "STUN",
                    0xFFFECC => "REVIVE",
                    _ => "UNKNOWN",
                };
                write!(
                    f,
                    " {command} RADIO:{} FM:{}",
                    self.destination().unwrap(),
                    self.source().unwrap()
                )
            }
            UnknownAhoy => {
                self.write_prefix(f, true)?;
                if self.is_encrypted() {
                    f.write_str(" ENCRYPTED")?;
                }
                write!(f, " AHOY {}", service_kind_label(field(bits, 28, 32)))?;
                write!(
                    f,
                    " TO:{} FM:{}",
                    self.destination().unwrap(),
                    self.source().unwrap()
                )
            }
            Announcement => {
                self.write_prefix(f, true)?;
                write!(
                    f,
                    " ANNOUNCEMENT {}",
                    announcement_type_label(field(bits, 16, 21))
                )
            }
            AdjacentSiteInformation => {
                let neighbor = SystemIdentityCode::new(bits, 21, false);
                let sic = self.system_identity_code();
                self.write_prefix(f, true)?;
                write!(
                    f,
                    " {} NEIGHBOR NETWORK:{} SITE:{}",
                    sic.model_label(),
                    neighbor.network,
                    neighbor.site
                )?;
                write!(f, " {}", self.channel.unwrap())?;
                write!(f, " THIS NETWORK:{} SITE:{}", sic.network, sic.site)
            }
            AnnounceChannelFrequency => {
                self.write_prefix(f, false)?;
                f.write_str(" ANNOUNCE CHANNEL")?;
                if let Some(parameters) = &self.absolute {
                    write!(f, "{} CC:{}", parameters.channel(), parameters.color_code())?;
                }
                Ok(())
            }
            AnnounceWithdrawTscc => {
                self.write_prefix(f, false)?;
                let add = |flag: usize| {
                    if get(bits, flag) {
                        " ADD CHAN:"
                    } else {
                        " WITHDRAW CHAN:"
                    }
                };
                if self.absolute.is_some() || field(bits, 56, 68) != 0 {
                    let cc = match &self.absolute {
                        Some(parameters) => parameters.color_code(),
                        None => field(bits, 25, 29),
                    };
                    write!(f, "{}{} CC:{}", add(33), self.channel.unwrap(), cc)?;
                }
                if field(bits, 68, 80) != 0 {
                    write!(
                        f,
                        "{}{} CC:{}",
                        add(34),
                        DmrChannel::tier3(field(bits, 68, 80), 1),
                        field(bits, 29, 33)
                    )?;
                }
                self.write_this_site(f, &self.system_identity_code())
            }
            CallTimerParameters => {
                self.write_prefix(f, false)?;
                write!(
                    f,
                    " CALL TIMERS EMERG:{}",
                    emergency_timer(field(bits, 21, 30))
                )?;
                write!(f, " PACKET:{}", packet_timer(field(bits, 30, 35)))?;
                write!(f, " MS-MS:{}", call_timer(field(bits, 56, 68)))?;
                write!(f, " MS-LINE:{}", call_timer(field(bits, 68, 80)))?;
                self.write_this_site(f, &self.system_identity_code())
            }
            LocalTime => {
                self.write_prefix(f, false)?;
                // SDRTrunk prints a java.util.Date in the JVM's time zone; this is the raw fields.
                let utc = field(bits, 30, 35);
                let hour = (field(bits, 56, 61) + if utc == 31 { 0 } else { utc }) % 24;
                write!(
                    f,
                    " LOCAL TIME:{:02}-{:02} {:02}:{:02}:{:02}",
                    field(bits, 26, 29) * 2 + u32::from(get(bits, 39)),
                    field(bits, 21, 26),
                    hour,
                    field(bits, 61, 67),
                    field(bits, 67, 73)
                )?;
                self.write_this_site(f, &self.system_identity_code())
            }
            MassRegistration => {
                self.write_prefix(f, false)?;
                f.write_str(" MASS REGISTRATION")?;
                if field(bits, 56, 80) != 0 {
                    write!(f, " TO:{}", self.destination().unwrap())?;
                }
                self.write_this_site(f, &self.system_identity_code())
            }
            VoteNowAdvice => {
                let voted = SystemIdentityCode::new(bits, 21, false);
                let sic = self.system_identity_code();
                self.write_prefix(f, false)?;
                write!(f, " VOTED NETWORK:{} SITE:{}", voted.network, voted.site)?;
                write!(f, " CHAN:{}", self.channel.unwrap())?;
                write!(
                    f,
                    " THIS {} NETWORK:{} SITE:{}",
                    sic.model_label(),
                    sic.network,
                    sic.site
                )
            }
            BroadcastTalkgroupVoiceChannelGrant => {
                self.write_grant(f, "BROADCAST TALKGROUP VOICE CHANNEL GRANT", false, false)
            }
            TalkgroupVoiceChannelGrant => {
                self.write_grant(f, "TALKGROUP VOICE CHANNEL GRANT", false, true)
            }
            PrivateVoiceChannelGrant => {
                self.write_grant(f, "PRIVATE VOICE CHANNEL GRANT", false, false)
            }
            DuplexPrivateVoiceChannelGrant => {
                self.write_grant(f, "DUPLEX PRIVATE VOICE CHANNEL GRANT", false, false)
            }
            PrivateDataChannelGrant => {
                self.write_grant(f, "PRIVATE DATA CHANNEL GRANT", true, false)
            }
            DuplexPrivateDataChannelGrant => {
                self.write_grant(f, "DUPLEX PRIVATE DATA CHANNEL GRANT", false, false)
            }
            TalkgroupDataChannelGrant => {
                self.write_grant(f, "TALKGROUP DATA CHANNEL GRANT", true, false)
            }
            Unknown | UnknownMulti => self.write_unknown(f),
            MbcHeader => {
                write!(f, "CC:{}", self.burst.color_code)?;
                if self.burst.has_ras() {
                    write!(f, " RAS:{}", self.burst.reserved)?;
                }
                if !self.burst.valid {
                    f.write_str(" [CRC ERROR]")?;
                }
                if self.is_encrypted() {
                    f.write_str(" ENCRYPTED")?;
                }
                f.write_str(" MULTI-BLOCK CSBK HEADER")?;
                match self.vendor() {
                    Vendor::Standard => {}
                    Vendor::Unknown => write!(f, " VENDOR:UNKNOWN ({})", field(bits, 8, 16))?,
                    vendor => write!(f, " {vendor}")?,
                }
                match self.opcode() {
                    Opcode::Unknown => write!(f, " UNKNOWN CSBKO:{}", field(bits, 2, 8))?,
                    op if op.label() == "HYTERA 08 ANNOUNCEMENT" => write!(
                        f,
                        " ANNOUNCEMENT:{}",
                        announcement_type_label(field(bits, 16, 21))
                    )?,
                    op => write!(f, " {op}")?,
                }
                write!(f, " MSG:{}", hex(bits))
            }
        }
    }
}

#[cfg(test)]
#[path = "csbk_tests.rs"]
mod tests;
