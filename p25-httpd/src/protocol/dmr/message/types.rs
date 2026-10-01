//! Field types and labels shared by DMR messages: ports of SDRTrunk
//! `module/decode/dmr/message/type/*`, `data/csbk/Opcode`, `data/lc/LCOpcode`,
//! the DMR identifiers (`identifier/*`) and channels (`channel/*`).
//!
//! Labels are SDRTrunk's `toString()` text, so messages print the same.

use std::fmt;

use super::bits::field;

/// Manufacturer feature ID. Ports `type/Vendor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Standard,
    FyldeMicro,
    ProdElSpa,
    MotorolaConnectPlus,
    RadioDataGmbh,
    Hytera8,
    Aselsan,
    Kirisun,
    DmrAssociation,
    Sepura,
    ItaliaRedCross,
    MotorolaCapacityPlus,
    MinisteroDellInterno,
    EmcCommSrl28,
    EmcCommSrl32,
    JvcKenwood,
    RadioActivitySrl,
    ZteTrunking,
    Tait,
    Hytera68,
    VertexStandard,
    Simoco,
    Test,
    Hytera88,
    Unknown,
}

const VENDORS: [(Vendor, u32, &str); 24] = [
    (Vendor::Standard, 0, "STANDARD"),
    (Vendor::FyldeMicro, 4, "FYLDE MICRO"),
    (Vendor::ProdElSpa, 5, "PROD-EL SPA"),
    (Vendor::MotorolaConnectPlus, 6, "MOTOROLA CON+"),
    (Vendor::RadioDataGmbh, 7, "RADIO DATA GMBH"),
    (Vendor::Hytera8, 8, "HYTERA"),
    (Vendor::Aselsan, 9, "ASELSAN"),
    (Vendor::Kirisun, 10, "KIRISUN"),
    (Vendor::DmrAssociation, 11, "DMR ASSOCIATION"),
    (Vendor::Sepura, 12, "SEPURA"),
    (Vendor::ItaliaRedCross, 13, "ITALIA RED CROSS"),
    (Vendor::MotorolaCapacityPlus, 16, "MOTOROLA CAP+"),
    (Vendor::MinisteroDellInterno, 19, "ITALY MIN INTERIOR"),
    (Vendor::EmcCommSrl28, 28, "EMC COMM SRL 28"),
    (Vendor::EmcCommSrl32, 32, "EMC COMM SRL 32"),
    (Vendor::JvcKenwood, 51, "JVC-KENWOOD"),
    (Vendor::RadioActivitySrl, 60, "RADIO ACTIVITY SRL"),
    (Vendor::ZteTrunking, 84, "ZTE TRUNKING"),
    (Vendor::Tait, 88, "TAIT"),
    (Vendor::Hytera68, 104, "HYTERA"),
    (Vendor::VertexStandard, 119, "VERTEX STANDARD"),
    (Vendor::Simoco, 120, "SIMOCO"),
    (Vendor::Test, 121, "TEST"),
    (Vendor::Hytera88, 136, "HYTERA"),
];

impl Vendor {
    /// Ports `Vendor.fromValue()`.
    pub fn from_value(value: u32) -> Vendor {
        VENDORS
            .iter()
            .find(|v| v.1 == value)
            .map_or(Vendor::Unknown, |v| v.0)
    }

    pub fn label(self) -> &'static str {
        VENDORS
            .iter()
            .find(|v| v.0 == self)
            .map_or("UNKNOWN", |v| v.2)
    }
}

impl fmt::Display for Vendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// CSBK opcode (CSBKO) per vendor. Ports `data/csbk/Opcode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    StandardFeatureNotSupported,
    StandardUnitToUnitVoiceServiceRequest,
    StandardUnitToUnitVoiceServiceResponse,
    StandardChannelTiming,
    StandardAloha,
    StandardUnifiedDataTransportOutboundHeader,
    StandardUnifiedDataTransportInboundHeader,
    StandardAhoy,
    StandardActivation,
    StandardRandomAccessServiceRequest,
    StandardAcknowledgeResponseOutboundTscc,
    StandardAcknowledgeResponseInboundTscc,
    StandardAcknowledgeResponseOutboundPayload,
    StandardAcknowledgeResponseInboundPayload,
    StandardUnifiedDataTransportForDgnaOutboundHeader,
    StandardUnifiedDataTransportForDgnaInboundHeader,
    StandardNegativeAcknowledgeResponse,
    StandardAnnouncement,
    StandardMaintenance,
    StandardClear,
    StandardProtect,
    StandardPrivateVoiceChannelGrant,
    StandardTalkgroupVoiceChannelGrant,
    StandardBroadcastTalkgroupVoiceChannelGrant,
    StandardPrivateDataChannelGrantSingleItem,
    StandardTalkgroupDataChannelGrantSingleItem,
    StandardDuplexPrivateVoiceChannelGrant,
    StandardDuplexPrivateDataChannelGrant,
    StandardPrivateDataChannelGrantMultiItem,
    StandardTalkgroupDataChannelGrantMultiItem,
    StandardMoveTscc,
    StandardPreamble,
    /// A vendor (Motorola / Hytera) opcode: its table index.
    Vendor(usize),
    Unknown,
}

use Opcode::*;

const STANDARD_OPCODES: [(Opcode, u32, &str); 32] = [
    (StandardFeatureNotSupported, 3, "FEATURE NOT SUPPORTED"),
    (
        StandardUnitToUnitVoiceServiceRequest,
        4,
        "UNIT TO UNIT VOICE SERVICE REQUEST",
    ),
    (
        StandardUnitToUnitVoiceServiceResponse,
        5,
        "UNIT TO UNIT VOICE SERVICE RESPONSE",
    ),
    (StandardChannelTiming, 7, "CHANNEL TIMING"),
    (StandardAloha, 25, "ALOHA"),
    (
        StandardUnifiedDataTransportOutboundHeader,
        26,
        "UNIFIED DATA TRANSPORT OUTBOUND HEADER",
    ),
    (
        StandardUnifiedDataTransportInboundHeader,
        27,
        "UNIFIED DATA TRANSPORT INBOUND HEADER",
    ),
    (StandardAhoy, 28, "AHOY"),
    (StandardActivation, 30, "ACTIVATION"),
    (
        StandardRandomAccessServiceRequest,
        31,
        "RANDOM ACCESS SERVICE REQUEST",
    ),
    (
        StandardAcknowledgeResponseOutboundTscc,
        32,
        "ACKNOWLEDGE RESPONSE OUTBOUND TSCC",
    ),
    (
        StandardAcknowledgeResponseInboundTscc,
        33,
        "ACKNOWLEDGE RESPONSE INBOUND TSCC",
    ),
    (
        StandardAcknowledgeResponseOutboundPayload,
        34,
        "ACKNOWLEDGE RESPONSE OUTBOUND PAYLOAD",
    ),
    (
        StandardAcknowledgeResponseInboundPayload,
        35,
        "ACKNOWLEDGE RESPONSE INBOUND PAYLOAD",
    ),
    (
        StandardUnifiedDataTransportForDgnaOutboundHeader,
        36,
        "UNIFIED DATA TRANSPORT OUTBOUND HEADER",
    ),
    (
        StandardUnifiedDataTransportForDgnaInboundHeader,
        37,
        "UNIFIED DATA TRANSPORT OUTBOUND HEADER",
    ),
    (
        StandardNegativeAcknowledgeResponse,
        38,
        "NEGATIVE ACKNOWLEDGE RESPONSE",
    ),
    (StandardAnnouncement, 40, "ANNOUNCEMENT"),
    (StandardMaintenance, 42, "MAINTENANCE"),
    (StandardClear, 46, "CLEAR"),
    (StandardProtect, 47, "PROTECT"),
    (
        StandardPrivateVoiceChannelGrant,
        48,
        "PRIVATE VOICE CHANNEL GRANT",
    ),
    (
        StandardTalkgroupVoiceChannelGrant,
        49,
        "TALKGROUP VOICE CHANNEL GRANT",
    ),
    (
        StandardBroadcastTalkgroupVoiceChannelGrant,
        50,
        "BROADCAST TALKGROUP VOICE CHANNEL GRANT",
    ),
    (
        StandardPrivateDataChannelGrantSingleItem,
        51,
        "PRIVATE DATA CHANNEL GRANT SINGLE ITEM",
    ),
    (
        StandardTalkgroupDataChannelGrantSingleItem,
        52,
        "TALKGROUP DATA CHANNEL GRANT SINGLE ITEM",
    ),
    (
        StandardDuplexPrivateVoiceChannelGrant,
        53,
        "DUPLEX PRIVATE VOICE CHANNEL GRANT",
    ),
    (
        StandardDuplexPrivateDataChannelGrant,
        54,
        "DUPLEX PRIVATE DATA CHANNEL GRANT",
    ),
    (
        StandardPrivateDataChannelGrantMultiItem,
        55,
        "PRIVATE DATA CHANNEL GRANT MULTI ITEM",
    ),
    (
        StandardTalkgroupDataChannelGrantMultiItem,
        56,
        "TALKGROUP DATA CHANNEL GRANT MULTI ITEM",
    ),
    (StandardMoveTscc, 57, "MOVE TSCC"),
    (StandardPreamble, 61, "PREAMBLE"),
];

/// Vendor opcodes SDRTrunk knows (printed by name; their messages decode as unknown CSBKs here).
const VENDOR_OPCODES: [(Vendor, u32, &str); 33] = [
    (Vendor::MotorolaConnectPlus, 1, "NEIGHBOR REPORT"),
    (Vendor::MotorolaConnectPlus, 3, "VOICE CHANNEL USER"),
    (Vendor::MotorolaConnectPlus, 6, "DATA CHANNEL GRANT"),
    (Vendor::MotorolaConnectPlus, 10, "CSBKO 10"),
    (Vendor::MotorolaConnectPlus, 12, "TERMINATE CHANNEL GRANT"),
    (Vendor::MotorolaConnectPlus, 16, "CSBKO 16"),
    (Vendor::MotorolaConnectPlus, 17, "REGISTRATION REQUEST"),
    (Vendor::MotorolaConnectPlus, 18, "REGISTRATION RESPONSE"),
    (Vendor::MotorolaConnectPlus, 24, "TALKGROUP AFFILIATION"),
    (
        Vendor::MotorolaConnectPlus,
        28,
        "ENHANCED DATA REVERT WINDOW ANNOUNCEMENT",
    ),
    (
        Vendor::MotorolaConnectPlus,
        29,
        "ENHANCED DATA REVERT WINDOW GRANT",
    ),
    (Vendor::MotorolaCapacityPlus, 25, "CAP MAX ALOHA"),
    (
        Vendor::MotorolaCapacityPlus,
        33,
        "CAP MAX CHAN UPD OPEN MODE",
    ),
    (
        Vendor::MotorolaCapacityPlus,
        34,
        "CAP MAX CHAN UPD ADV MODE",
    ),
    (Vendor::MotorolaCapacityPlus, 31, "CALL ALERT"),
    (Vendor::MotorolaCapacityPlus, 32, "CALL ALERT ACK"),
    (
        Vendor::MotorolaCapacityPlus,
        41,
        "ENHANCED DATA REVERT WINDOW ANNOUNCEMENT",
    ),
    (
        Vendor::MotorolaCapacityPlus,
        42,
        "ENHANCED DATA REVERT WINDOW GRANT",
    ),
    (Vendor::MotorolaCapacityPlus, 59, "NEIGHBOR REPORT"),
    (Vendor::MotorolaCapacityPlus, 60, "CSBKO 60"),
    (Vendor::MotorolaCapacityPlus, 61, "PREAMBLE"),
    (Vendor::MotorolaCapacityPlus, 62, "SITE STATUS"),
    (Vendor::Hytera8, 32, "HYTERA 08 ACKNOWLEDGE"),
    (Vendor::Hytera8, 40, "HYTERA 08 ANNOUNCEMENT"),
    (Vendor::Hytera8, 44, "HYTERA 08 CSBKO 44"),
    (Vendor::Hytera8, 47, "HYTERA 08 CSBKO 47"),
    (Vendor::Hytera68, 10, "HYTERA 68 XPT SITE STATE"),
    (Vendor::Hytera68, 11, "HYTERA 68 XPT ADJACENT SITE"),
    (Vendor::Hytera68, 25, "HYTERA 68 ALOHA"),
    (Vendor::Hytera68, 32, "HYTERA 68 ACKNOWLEDGE"),
    (Vendor::Hytera68, 40, "HYTERA 68 ANNOUNCEMENT"),
    (Vendor::Hytera68, 61, "HYTERA 68 XPT PREAMBLE"),
    (Vendor::Hytera68, 62, "HYTERA 68 CSBKO 62"),
];

impl Opcode {
    /// Ports `Opcode.fromValue(value, vendor)`.
    pub fn from_value(value: u32, vendor: Vendor) -> Opcode {
        if vendor == Vendor::Standard {
            return STANDARD_OPCODES
                .iter()
                .find(|o| o.1 == value)
                .map_or(Unknown, |o| o.0);
        }
        VENDOR_OPCODES
            .iter()
            .position(|o| o.0 == vendor && o.1 == value)
            .map_or(Unknown, Opcode::Vendor)
    }

    pub fn label(self) -> &'static str {
        match self {
            Opcode::Vendor(i) => VENDOR_OPCODES[i].2,
            Unknown => "UNKNOWN",
            standard => STANDARD_OPCODES
                .iter()
                .find(|o| o.0 == standard)
                .map_or("UNKNOWN", |o| o.2),
        }
    }

    /// The 6-bit value, -1 for unknown.
    pub fn value(self) -> i32 {
        match self {
            Opcode::Vendor(i) => VENDOR_OPCODES[i].1 as i32,
            Unknown => -1,
            standard => STANDARD_OPCODES
                .iter()
                .find(|o| o.0 == standard)
                .map_or(-1, |o| o.1 as i32),
        }
    }
}

impl fmt::Display for Opcode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Link control opcode (FLCO / SLCO). Ports `data/lc/LCOpcode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LcOpcode(usize);

/// (full, vendor, value, label), SDRTrunk's order.
const LC_OPCODES: [(bool, Vendor, i32, &str); 36] = [
    (true, Vendor::Standard, 0, "GROUP VOICE CHANNEL USER"),
    (true, Vendor::Standard, 3, "UNIT-TO-UNIT VOICE CHANNEL USER"),
    (true, Vendor::Standard, 4, "TALKER ALIAS HEADER"),
    (true, Vendor::Standard, 5, "TALKER ALIAS BLOCK 1"),
    (true, Vendor::Standard, 6, "TALKER ALIAS BLOCK 2"),
    (true, Vendor::Standard, 7, "TALKER ALIAS BLOCK 3"),
    (true, Vendor::Standard, -1, "TALKER ALIAS COMPLETE"),
    (true, Vendor::Standard, 8, "GPS INFO"),
    (true, Vendor::Standard, 48, "TERMINATOR DATA"),
    (true, Vendor::Standard, -1, "FULL UNKNOWN"),
    (
        true,
        Vendor::MotorolaCapacityPlus,
        0,
        "GROUP VOICE CHANNEL USER",
    ),
    (
        true,
        Vendor::MotorolaCapacityPlus,
        4,
        "WAN GROUP VOICE CHANNEL USER",
    ),
    (
        true,
        Vendor::MotorolaCapacityPlus,
        16,
        "CAPMAX GROUP VOICE CHANNEL USER",
    ),
    (
        true,
        Vendor::MotorolaCapacityPlus,
        20,
        "CAPMAX TALKER ALIAS",
    ),
    (
        true,
        Vendor::MotorolaCapacityPlus,
        21,
        "CAPMAX TALKER ALIAS CONTINUATION",
    ),
    (
        true,
        Vendor::MotorolaCapacityPlus,
        32,
        "ENCRYPTED VOICE CHANNEL USER",
    ),
    (
        true,
        Vendor::MotorolaCapacityPlus,
        33,
        "ENCRYPTION PARAMETERS",
    ),
    (true, Vendor::Hytera68, 0, "HYTERA GROUP VOICE CHANNEL USER"),
    (
        true,
        Vendor::Hytera68,
        3,
        "HYTERA UNIT-TO-UNIT VOICE CHANNEL USER",
    ),
    (true, Vendor::Hytera68, 4, "HYTERA TALKER ALIAS HEADER"),
    (true, Vendor::Hytera68, 5, "HYTERA TALKER ALIAS BLOCK 1"),
    (true, Vendor::Hytera68, 6, "HYTERA TALKER ALIAS BLOCK 2"),
    (true, Vendor::Hytera68, 7, "HYTERA TALKER ALIAS BLOCK 3"),
    (true, Vendor::Hytera68, 8, "HYTERA GPS INFO"),
    (true, Vendor::Hytera68, 9, "HYTERA XPT CHANNEL GRANT"),
    (true, Vendor::Hytera68, 48, "HYTERA TERMINATOR"),
    (false, Vendor::Standard, 0, "NULL MESSAGE"),
    (false, Vendor::Standard, 1, "ACTIVITY UPDATE"),
    (
        false,
        Vendor::Standard,
        2,
        "CONTROL CHANNEL SYSTEM PARAMETERS",
    ),
    (
        false,
        Vendor::Standard,
        3,
        "TRAFFIC CHANNEL SYSTEM PARAMETERS",
    ),
    (
        false,
        Vendor::MotorolaCapacityPlus,
        15,
        "REST CHANNEL NOTIFICATION",
    ),
    (false, Vendor::Standard, 8, "STANDARD XPT CHANNEL"),
    (false, Vendor::Hytera68, 8, "HYTERA XPT CHANNEL"),
    (false, Vendor::Standard, 9, "TRAFFIC CHANNEL INFO"),
    (false, Vendor::Standard, 10, "CONTROL CHANNEL INFO"),
    (false, Vendor::Standard, -1, "UNKNOWN"),
];

impl LcOpcode {
    pub const FULL_STANDARD_GROUP_VOICE_CHANNEL_USER: LcOpcode = LcOpcode(0);
    pub const FULL_STANDARD_UNIT_TO_UNIT_VOICE_CHANNEL_USER: LcOpcode = LcOpcode(1);
    pub const FULL_STANDARD_TERMINATOR_DATA: LcOpcode = LcOpcode(8);
    pub const FULL_STANDARD_UNKNOWN: LcOpcode = LcOpcode(9);
    pub const FULL_ENCRYPTION_PARAMETERS: LcOpcode = LcOpcode(16);
    pub const SHORT_STANDARD_NULL_MESSAGE: LcOpcode = LcOpcode(26);
    pub const SHORT_STANDARD_ACTIVITY_UPDATE: LcOpcode = LcOpcode(27);
    pub const SHORT_STANDARD_CONTROL_CHANNEL_SYSTEM_PARAMETERS: LcOpcode = LcOpcode(28);
    pub const SHORT_STANDARD_TRAFFIC_CHANNEL_SYSTEM_PARAMETERS: LcOpcode = LcOpcode(29);
    pub const SHORT_CAPACITY_PLUS_REST_CHANNEL_NOTIFICATION: LcOpcode = LcOpcode(30);
    pub const SHORT_STANDARD_XPT_CHANNEL: LcOpcode = LcOpcode(31);
    pub const SHORT_HYTERA_XPT_CHANNEL: LcOpcode = LcOpcode(32);
    pub const SHORT_CONNECT_PLUS_TRAFFIC_CHANNEL: LcOpcode = LcOpcode(33);
    pub const SHORT_CONNECT_PLUS_CONTROL_CHANNEL: LcOpcode = LcOpcode(34);
    pub const SHORT_STANDARD_UNKNOWN: LcOpcode = LcOpcode(35);

    /// Ports `LCOpcode.fromValue(full, value, vendor)` (later entries win, as in its lookup map).
    pub fn from_value(full: bool, value: u32, vendor: Vendor) -> LcOpcode {
        LC_OPCODES
            .iter()
            .rposition(|o| o.0 == full && o.1 == vendor && o.2 == value as i32)
            .map_or(
                if full {
                    Self::FULL_STANDARD_UNKNOWN
                } else {
                    Self::SHORT_STANDARD_UNKNOWN
                },
                LcOpcode,
            )
    }

    pub fn label(self) -> &'static str {
        LC_OPCODES[self.0].3
    }

    /// The opcode value, -1 for unknown.
    pub fn value(self) -> i32 {
        LC_OPCODES[self.0].2
    }
}

impl fmt::Display for LcOpcode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Labels for small enums indexed by their value (SDRTrunk `values()[value]`).
fn indexed(labels: &'static [&'static str], value: u32) -> &'static str {
    labels.get(value as usize).copied().unwrap_or("UNKNOWN")
}

/// `type/Model` (enum names).
pub fn model_label(value: u32) -> &'static str {
    indexed(&["TINY", "SMALL", "LARGE", "HUGE"], value)
}

/// `type/PAR`.
pub fn par_label(value: u32) -> &'static str {
    indexed(
        &[
            "RESERVED",
            "MULTIPLE CONTROL CHANNELS - CAT A SUBSCRIBERS",
            "MULTIPLE CONTROL CHANNELS - CAT B SUBSCRIBERS",
            "SINGLE CONTROL CHANNEL - CAT A & B SUBSCRIBERS",
        ],
        value,
    )
}

/// `type/ServiceFunction`.
pub fn service_function_label(value: u32) -> &'static str {
    indexed(
        &[
            "ALL",
            "REGISTRATION/CHANNEL GRANTS ONLY",
            "REGISTRATION/NO CHANNEL GRANTS",
            "REGISTRATION ONLY",
        ],
        value,
    )
}

/// `type/Version`.
pub fn version_label(value: u32) -> &'static str {
    indexed(
        &[
            "1.0.0-1.5.1",
            "1.6.1",
            "1.7.1-1.9.1",
            "RESERVED 3",
            "RESERVED 4",
            "RESERVED 5",
            "RESERVED 6",
            "RESERVED 7",
        ],
        value,
    )
}

/// `type/ProtectKind`.
pub fn protect_kind_label(value: u32) -> &'static str {
    indexed(
        &[
            "DISABLE PTT",
            "ENABLE PTT",
            "ILLEGALLY PARKED",
            "ENABLE TARGET ID PTT ONLY",
        ],
        value,
    )
}

/// `type/AcknowledgeType`.
pub fn acknowledge_type_label(value: u32) -> &'static str {
    indexed(
        &[
            "ACKNOWLEDGED",
            "REJECTED/REFUSED/NOT ACKNOWLEDGED",
            "QUEUED",
            "WAIT",
        ],
        value,
    )
}

/// `type/Activity`.
pub fn activity_label(value: u32) -> &'static str {
    indexed(
        &[
            "IDLE",
            "RESERVED 1",
            "GROUP CSBK",
            "INDIV CSBK",
            "RESERVED 4",
            "RESERVED 5",
            "RESERVED 6",
            "RESERVED 7",
            "GROUP VOICE",
            "INDIV VOICE",
            "INDIV DATA",
            "GROUP DATA",
            "EMERG GROUP VOICE",
            "EMERG INDIV VOICE",
            "RESERVED 14",
            "RESERVED 15",
        ],
        value,
    )
}

/// `type/ServiceKind`.
pub fn service_kind_label(value: u32) -> &'static str {
    indexed(
        &[
            "INDIVIDUAL VOICE CALL SERVICE",
            "TALKGROUP VOICE CALL SERVICE",
            "INDIVIDUAL PACKET CALL SERVICE",
            "TALKGROUP PACKET CALL SERVICE",
            "INDIVIDUAL UDT SHORT DATA CALL SERVICE",
            "TALKGROUP UDT SHORT DATA CALL SERVICE",
            "UDT SHORT DATA POLLING SERVICE",
            "STATUS TRANSPORT SERVICE",
            "CALL DIVERSION SERVICE",
            "CALL ANSWER SERVICE",
            "FULL DUPLEX MS TO MS VOICE CALL SERVICE",
            "FULL DUPLEX MS TO MS PACKET CALL SERVICE",
            "RESERVED 12",
            "SUPPLEMENTARY SERVICE",
            "REGISTRATION OR RADIO CHECK SERVICE",
            "CANCEL CALL SERVICE",
        ],
        value,
    )
}

/// `type/ServiceType` (USB data).
pub fn service_type_label(value: u32) -> String {
    match value {
        0 => "LIP SHORT LOCATION REQUEST".to_string(),
        1..=7 => format!("RESERVED {value}"),
        8..=15 => format!("VENDOR SERVICE {}", value - 7),
        _ => "UNKNOWN".to_string(),
    }
}

/// `type/AnnouncementType`.
pub fn announcement_type_label(value: u32) -> String {
    match value {
        0 => "ANNOUNCE/WITHDRAW TSCC".into(),
        1 => "CALL TIMER PARAMETERS".into(),
        2 => "VOTE NOW ADVICE".into(),
        3 => "BROADCAST LOCAL TIME".into(),
        4 => "MASS REGISTRATION".into(),
        5 => "CHANNEL FREQUENCY".into(),
        6 => "NEIGHBOR SITE INFORMATION".into(),
        7 => "SITE INFORMATION".into(),
        8..=29 => format!("RESERVED {value}"),
        // SDRTrunk labels 31 "VENDOR SPECIFIC 30" too.
        30 | 31 => "VENDOR SPECIFIC 30".into(),
        _ => "UNKNOWN".into(),
    }
}

/// `type/DataPacketFormat`.
pub fn data_packet_format_label(value: u32) -> &'static str {
    match value {
        0 => "UNIFIED DATA TRANSPORT",
        1 => "RESPONSE PACKET",
        2 => "UNCONFIRMED DATA PACKET",
        3 => "CONFIRMED DATA PACKET",
        13 => "DEFINED SHORT DATA",
        14 => "RAW OR STATUS SHORT DATA",
        15 => "PROPRIETARY DATA PACKET",
        _ => "UNKNOWN",
    }
}

/// `type/Reason` (acknowledge reason codes); `None` is SDRTrunk's UNKNOWN.
pub fn reason(value: u32) -> Option<&'static str> {
    Some(match value {
        0x00 => "SERVICE_NOT SUPPORTED",
        0x11 => "LINE NOT SUPPORTED",
        0x12 => "MS REFUSED - STACK FULL",
        0x13 => "MS REFUSED - EQUIPMENT BUSY",
        0x14 => "REFUSED BY RECIPIENT",
        0x15 => "REFUSED-CUSTOM",
        0x16 => "DUPLEX NOT SUPPORTED BY MS",
        0x1F => "REFUSED-REASON UNKNOWN",
        0x20 => "SERVICE NOT SUPPORTED",
        0x21 => "REFUSED-NOT PERMISSION",
        0x22 => "REFUSED-SERVICE TEMPORARY UNAVAILABLE",
        0x23 => "REFUSED-SERVICE UNAVAILABLE",
        0x24 => "REFUSED-CALLED RADIO NOT REGISTERED",
        0x25 => "REFUSED-CALLED RADIO OFFLINE",
        0x26 => "REFUSED-CALLED RADIO HAS CALL DIVERSION",
        0x27 => "REFUSED-NETWORK CONGESTION",
        0x28 => "REFUSED-NETWORK NOT READY",
        0x29 => "REFUSED-CANNOT CANCEL CALL",
        0x2A => "REGISTRATION REFUSED",
        0x2B => "REGISTRATION DENIED",
        0x2C => "IP CONNECTION FAILED",
        0x2D => "REFUSED-RADIO NOT REGISTERED",
        0x2E => "CALLED PARTY BUSY",
        0x2F => "CALLED TALKGROUP NOT ALLOWED",
        0x30 => "CRC ERROR IN UDT UPLOAD",
        0x31 => "REFUSED DUPLEX CALL-NETWORK CONGESTION",
        0x3F => "REFUSED-REASON UNKNONW",
        0x44 => "MESSAGE ACCEPTED",
        0x45 => "CALLBACK",
        0x46 => "ALERTING BUT NOT READY",
        0x47 => "ACCEPTED FOR POLLING STATUS SERVICE",
        0x48 => "AUTHENTICATION RESPONSE",
        0x60 => "MESSAGE ACCEPTED",
        0x61 => "STORE AND FORWARD",
        0x62 => "REGISTRATION ACCEPTED",
        0x63 => "ACCEPTED FOR STATUS POLLING SERVICE",
        0x64 => "AUTHENTICATION RESPONSE",
        0x65 => "SUBSCRIPTION SERVICE REGISTRATION ACCEPTED",
        0xA0 => "QUEUED FOR RESOURCE",
        0xAA => "QUEUED FOR BUSY RADIO",
        0xE0 => "WAIT",
        _ => return None,
    })
}

/// `type/EncryptionAlgorithm` label for an algorithm value, `None` if unknown.
pub fn encryption_algorithm(value: u32) -> Option<&'static str> {
    Some(match value {
        0x00 => "NO ENCRYPTION",
        0x01 => "HYTERA BP",
        0x02 => "HYTERA RC4/EP",
        0x21 => "DMRA RC4/EP",
        0x24 => "DMRA AES128",
        0x25 => "DMRA AES256",
        0x26 => "HYTERA RC4/EP",
        _ => return None,
    })
}

/// `type/Tier3Gateway`: the label of a gateway radio ID, if it is one.
pub fn tier3_gateway(value: u32) -> Option<&'static str> {
    Some(match value {
        0xFFFEC0 => "PSTN GATEWAY",
        0xFFFEC1 => "PABX GATEWAY",
        0xFFFEC2 => "LINE GATEWAY",
        0xFFFEC3 => "IP GATEWAY",
        0xFFFEC4 => "SUPPLEMENTARY DATA SERVICE",
        0xFFFEC5 => "UDT SHORT DATA SERVICE",
        0xFFFEC6 => "REGISTRATION SERVICE",
        0xFFFEC7 => "CALL DIVERSION TO MS GATEWAY",
        0xFFFEC9 => "CALL DIVERSION CANCELLATION",
        0xFFFECA => "TRUNKING SYSTEM CONTROLLER",
        0xFFFECB => "SYSTEM DISPATCHER",
        0xFFFECC => "MS STUN/REVIVE",
        0xFFFECD => "AUTHENTICATION",
        0xFFFECE => "CALL DIVERSION TO TALKGROUP GATEWAY",
        0xFFFECF => "MS KILL",
        0xFFFED0 => "PSTN-D GATEWAY",
        0xFFFED1 => "PABX-D GATEWAY",
        0xFFFED2 => "LINE-D GATEWAY",
        0xFFFED3 => "SYSTEM DISPATCHER-D",
        0xFFFED4 => "ALL RADIOS/TALKGROUPS",
        0xFFFED5 => "IP-D GATEWAY",
        0xFFFED6 => "DYNAMIC GROUP NUMBER ASSIGNMENT",
        0xFFFED7 => "TALKGROUP SUBSCRIBE/ATTACH SERVICE",
        0xFFFFFD => "ALL RADIOS AT SITE",
        0xFFFFFE => "ALL RADIOS IN ZONE",
        0xFFFFFF => "ALL RADIOS IN SYSTEM",
        _ => return None,
    })
}

/// Who a radio or talkgroup ID is, as SDRTrunk prints it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Address {
    /// `DMRRadio`: the number.
    Radio(u32),
    /// `DmrTier3Radio`: the number, or the gateway's name.
    Tier3Radio(u32),
    /// `DMRTalkgroup`.
    Talkgroup(u32),
}

impl Address {
    pub fn value(self) -> u32 {
        match self {
            Address::Radio(v) | Address::Tier3Radio(v) | Address::Talkgroup(v) => v,
        }
    }

    pub fn is_talkgroup(self) -> bool {
        matches!(self, Address::Talkgroup(_))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Address::Tier3Radio(v) => match tier3_gateway(v) {
                Some(label) => f.write_str(label),
                None => write!(f, "{v}"),
            },
            Address::Radio(v) | Address::Talkgroup(v) => write!(f, "{v}"),
        }
    }
}

/// A traffic or control channel: logical channel number and timeslot, plus
/// frequencies once known. Ports `DMRTier3Channel` / `DMRAbsoluteChannel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmrChannel {
    pub lcn: u16,
    /// 1 or 2.
    pub timeslot: u8,
    pub downlink_hz: Option<u64>,
    pub uplink_hz: Option<u64>,
    /// From MBC absolute channel parameters (frequencies on air) rather than the LCN map.
    pub absolute: bool,
}

impl DmrChannel {
    pub fn tier3(lcn: u32, timeslot: u8) -> Self {
        DmrChannel {
            lcn: lcn as u16,
            timeslot,
            downlink_hz: None,
            uplink_hz: None,
            absolute: false,
        }
    }

    /// `DMRTier3Channel.getChannelId()`: LCN * 2 + timeslot.
    pub fn channel_id(&self) -> u32 {
        u32::from(self.lcn) * 2 + u32::from(self.timeslot)
    }
}

impl fmt::Display for DmrChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.absolute {
            let mhz = self.downlink_hz.unwrap_or(0) as f64 / 1e6;
            write!(f, "{} {}", self.lcn, java_double(mhz))
        } else {
            write!(f, " LCN:{} CHANID:{}", self.lcn, self.channel_id())
        }
    }
}

/// A double as Java's `Double.toString()` prints it in the ranges used here.
pub fn java_double(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e7 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

/// MBC continuation block absolute channel parameters. Ports `type/AbsoluteChannelParameters`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsoluteChannelParameters {
    pub bits: Vec<u8>,
    pub timeslot: u8,
}

impl AbsoluteChannelParameters {
    pub fn color_code(&self) -> u32 {
        field(&self.bits, 12, 16)
    }

    pub fn channel(&self) -> DmrChannel {
        let mhz = |range: (usize, usize), hz: (usize, usize)| {
            u64::from(field(&self.bits, range.0, range.1)) * 1_000_000
                + u64::from(field(&self.bits, hz.0, hz.1)) * 125
        };
        DmrChannel {
            lcn: field(&self.bits, 22, 34) as u16,
            timeslot: self.timeslot,
            downlink_hz: Some(mhz((57, 67), (67, 80))),
            uplink_hz: Some(mhz((34, 44), (44, 57))),
            absolute: true,
        }
    }
}

/// System identity code: model, network and site. Ports `type/SystemIdentityCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemIdentityCode {
    pub model: u32,
    pub network: u32,
    pub site: u32,
    /// PAR subfield, when the structure carries one.
    pub par: Option<u32>,
}

impl SystemIdentityCode {
    /// Reads the 14 (or 16, with `has_par`) bits at `offset`.
    pub fn new(bits: &[u8], offset: usize, has_par: bool) -> Self {
        let model = field(bits, offset, offset + 2);
        let (net_bits, site_bits) = match model {
            0 => (9, 3),
            1 => (7, 5),
            2 => (4, 8),
            _ => (2, 10),
        };
        let network = field(bits, offset + 2, offset + 2 + net_bits);
        let site = field(
            bits,
            offset + 2 + net_bits,
            offset + 2 + net_bits + site_bits,
        );
        let par = if has_par {
            Some(field(bits, offset + 14, offset + 16))
        } else {
            None
        };
        SystemIdentityCode {
            model,
            network,
            site,
            par,
        }
    }

    pub fn model_label(&self) -> &'static str {
        model_label(self.model)
    }

    /// `PAR.isMultipleControlChannels()`.
    pub fn is_multiple_control_channels(&self) -> bool {
        matches!(self.par, Some(1) | Some(2))
    }
}

/// Voice call service options byte. Ports `type/ServiceOptions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceOptions(pub u32);

impl ServiceOptions {
    pub fn is_emergency(self) -> bool {
        self.0 & 0x80 != 0
    }

    pub fn is_encrypted(self) -> bool {
        self.0 & 0x40 != 0
    }

    pub fn is_broadcast(self) -> bool {
        self.0 & 0x08 != 0
    }

    pub fn priority(self) -> u32 {
        self.0 & 0x03
    }
}

impl fmt::Display for ServiceOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut flags: Vec<String> = Vec::new();
        for (mask, label) in [
            (0x80, "EMERGENCY"),
            (0x40, "ENCRYPTED"),
            (0x08, "BROADCAST"),
            (0x04, "OVCM"),
            (0x20, "RSVD1"),
            (0x10, "RSVD2"),
        ] {
            if self.0 & mask == mask {
                flags.push(label.to_string());
            }
        }
        if self.priority() > 0 {
            flags.push(format!("PRIORITY-{}", self.priority()));
        }
        write!(f, "SERVICE OPTIONS [{}]", flags.join(","))
    }
}
