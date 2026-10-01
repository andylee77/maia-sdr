//! Data bursts: ports of SDRTrunk `message/data/DMRDataMessageFactory`,
//! `DataMessage`, `IDLEMessage`, `UnknownDataMessage`, the headers
//! (`header/DataHeader` for all data headers, `PiHeader`, `VoiceHeader`),
//! `terminator/Terminator`, `mbc/MBCContinuationBlock`, `usb/USBData` and
//! `block/DataBlock*`.

use std::fmt;

use super::bits::{field, hex};
use super::crc_mask::DmrCrcMaskManager;
use super::csbk::{self, Csbk, CsbkKind};
use super::lc::{self, FullLc};
use super::types::{data_packet_format_label, service_type_label};
use super::DmrMessage;
use crate::protocol::dmr::fec::bptc_196_96;
use crate::protocol::dmr::fec::cach::Cach;
use crate::protocol::dmr::fec::crc;
use crate::protocol::dmr::fec::slot_type::{DataType, SlotType};
use crate::protocol::dmr::sync::DmrSyncPattern;

/// What every data burst carries (SDRTrunk `DMRBurst` + `DataMessage`).
#[derive(Debug, Clone)]
pub struct DataBurst {
    pub pattern: DmrSyncPattern,
    pub timeslot: u8,
    pub timestamp_ms: u64,
    pub cach: Cach,
    /// Slot type colour code.
    pub color_code: u8,
    /// Slot type data type.
    pub data_type: DataType,
    /// Payload: the 96 BPTC(196,96) info bits (the raw 288-bit burst for rate 1 data).
    pub bits: Vec<u8>,
    /// SDRTrunk's RAS value: the BPTC reserved bits it appends at 96..99.
    pub reserved: u8,
    pub valid: bool,
}

impl DataBurst {
    /// `DataMessage.hasRAS()`.
    pub fn has_ras(&self) -> bool {
        self.reserved != 0
    }

    /// `SlotType.toString()`: "CC:0 CSBK".
    pub fn slot_type(&self) -> String {
        format!("CC:{} {}", self.color_code, self.data_type.label())
    }
}

/// Rate of a data continuation block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataRate {
    /// Rate 1/2, BPTC(196,96) (`DataBlock1_2Rate`).
    Half,
    /// Rate 3/4, trellis coded (`DataBlock3_4Rate`); the trellis is not decoded here.
    ThreeQuarter,
    /// Rate 1, uncoded (`DataBlock1Rate`).
    Full,
}

/// Builds the message for a data burst from its slot type. Ports `DMRDataMessageFactory.create()`.
pub fn create(
    pattern: DmrSyncPattern,
    burst_bits: &[u8; 288],
    cach: Cach,
    timestamp_ms: u64,
    timeslot: u8,
    masks: &mut DmrCrcMaskManager,
) -> DmrMessage {
    // SDRTrunk's Golay decoder never reports failure; fall back to the raw bits.
    let (color_code, data_type) = match SlotType::get_slot_type(burst_bits) {
        Some(st) => (st.color_code, st.data_type),
        None => (
            field(burst_bits, 122, 126) as u8,
            DataType::from_value(field(burst_bits, 126, 130) as u8),
        ),
    };
    let base = |bits: Vec<u8>, reserved: u8, valid: bool| DataBurst {
        pattern,
        timeslot,
        timestamp_ms,
        cach,
        color_code,
        data_type,
        bits,
        reserved,
        valid,
    };

    match data_type {
        DataType::Rate3Of4Data => {
            return DmrMessage::DataBlock(base(Vec::new(), 0, true), DataRate::ThreeQuarter)
        }
        DataType::Rate1Data => {
            let reserved = field(burst_bits, 96, 99) as u8;
            return DmrMessage::DataBlock(
                base(burst_bits.to_vec(), reserved, true),
                DataRate::Full,
            );
        }
        _ => {}
    }

    let extracted = bptc_196_96::extract(&bptc_196_96::payload_from_burst(burst_bits));
    let bptc_ok = extracted.corrected.is_some();
    let mut data = base(extracted.bits.to_vec(), extracted.reserved, bptc_ok);
    let corrected = extracted.corrected.map_or(-2, |n| n as i32);

    match data_type {
        DataType::SlotIdle => DmrMessage::Idle(data),
        DataType::Csbk => {
            let mut csbk = csbk::create(data);
            if !bptc_ok {
                csbk.burst.valid = false;
            }
            DmrMessage::Csbk(csbk)
        }
        DataType::UsbData => {
            data.valid =
                crc::correct_ccitt80(&mut data.bits, crc::USB_DATA_CRC_MASK).is_some() && bptc_ok;
            DmrMessage::UsbData(data)
        }
        DataType::MbcEncHeader | DataType::MbcHeader => DmrMessage::Csbk(Csbk {
            burst: data,
            kind: CsbkKind::MbcHeader,
            blocks: Vec::new(),
            absolute: None,
            channel: None,
        }),
        DataType::ChannelControlEncHeader | DataType::PiHeader => {
            let lc = lc::create_full_encryption(data.bits.clone(), timestamp_ms, timeslot);
            DmrMessage::PiHeader(data, lc)
        }
        DataType::VoiceHeader => {
            let lc = lc::create_full(
                data.bits.clone(),
                timestamp_ms,
                timeslot,
                false,
                corrected,
                masks,
            );
            data.bits.clone_from(&lc.bits);
            DmrMessage::VoiceHeader(data, lc)
        }
        DataType::DataEncHeader | DataType::DataHeader => {
            let crc_ok = crc::correct_ccitt80(&mut data.bits, crc::DATA_HEADER_CRC_MASK).is_some();
            data.valid = crc_ok && bptc_ok;
            DmrMessage::DataHeader(data)
        }
        DataType::Tlc => {
            let lc = lc::create_full(
                data.bits.clone(),
                timestamp_ms,
                timeslot,
                true,
                corrected,
                masks,
            );
            data.bits.clone_from(&lc.bits);
            DmrMessage::Terminator(data, lc)
        }
        DataType::Rate1Of2Data => DmrMessage::DataBlock(data, DataRate::Half),
        // MBCContinuationBlock.isValid() is always true.
        DataType::MbcBlock => {
            data.valid = true;
            DmrMessage::MbcContinuation(data)
        }
        DataType::Reserved15 | DataType::Rate3Of4Data | DataType::Rate1Data => {
            DmrMessage::UnknownData(data)
        }
    }
}

/// `IDLEMessage.toString()`.
pub fn fmt_idle(data: &DataBurst, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "CC:{}", data.color_code)?;
    if data.has_ras() {
        write!(f, " RAS:{}", data.reserved)?;
    }
    if !data.valid {
        f.write_str(" [CRC-ERROR]")?;
    }
    f.write_str(" IDLE")
}

/// `DataHeader.toString()`.
pub fn fmt_data_header(data: &DataBurst, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "CC:{}", data.color_code)?;
    if !data.valid {
        f.write_str(" [CRC ERROR]")?;
    }
    write!(
        f,
        " DATA HEADER FORMAT:{}",
        data_packet_format_label(field(&data.bits, 4, 8))
    )?;
    write!(f, " MSG:{}", hex(&data.bits))
}

/// `PiHeader.toString()`.
pub fn fmt_pi_header(data: &DataBurst, lc: &FullLc, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if !data.valid {
        f.write_str(" [CRC ERROR]")?;
    }
    f.write_str(&data.slot_type())?;
    if data.has_ras() {
        write!(f, " RAS:{} ", data.reserved)?;
    }
    write!(f, " {lc}")
}

/// `VoiceHeader.toString()`.
pub fn fmt_voice_header(data: &DataBurst, lc: &FullLc, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if data.has_ras() {
        write!(f, "RAS:{} ", data.reserved)?;
    }
    write!(f, "CC:{} VOICE HEADER {lc}", data.color_code)
}

/// `DataMessageWithLinkControl.toString()` (the terminator).
pub fn fmt_terminator(data: &DataBurst, lc: &FullLc, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if data.has_ras() {
        write!(f, "RAS:{} ", data.reserved)?;
    }
    write!(f, "{} {lc}", data.slot_type())
}

/// `MBCContinuationBlock.toString()`.
pub fn fmt_mbc_continuation(data: &DataBurst, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "CC:{}", data.color_code)?;
    if data.has_ras() {
        write!(f, " RAS:{}", data.reserved)?;
    }
    f.write_str(" MULTI-BLOCK CSBK CONTINUATION")?;
    if data.bits[0] == 1 {
        f.write_str("-FINAL")?;
    }
    write!(f, " MSG:{}", hex(&data.bits))
}

/// `DataBlock1_2Rate` / `DataBlock3_4Rate` / `DataBlock1Rate` `toString()`.
pub fn fmt_data_block(data: &DataBurst, rate: DataRate, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let (name, end) = match rate {
        DataRate::Half => ("RATE 1/2", 96),
        DataRate::ThreeQuarter => ("RATE 3/4", 144),
        DataRate::Full => ("RATE 1/1", 192),
    };
    write!(f, "CC:{}", data.color_code)?;
    if data.has_ras() {
        write!(f, " RAS:{}", data.reserved)?;
    }
    if !data.valid {
        f.write_str(" [CRC ERROR]")?;
    }
    let end = end.min(data.bits.len());
    let start = 16.min(end);
    write!(f, " {name} DATA CONFIRMED:{}", hex(&data.bits[start..end]))?;
    write!(f, " UNCONFIRMED:{}", hex(&data.bits[..end]))
}

/// `USBData.toString()`.
pub fn fmt_usb_data(data: &DataBurst, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if !data.valid {
        f.write_str("[CRC-ERROR] ")?;
    }
    write!(f, "CC:{}", data.color_code)?;
    if data.has_ras() {
        write!(f, " RAS:{}", data.reserved)?;
    }
    write!(f, " USB DATA BLOCK TO:{}", field(&data.bits, 56, 80))?;
    write!(
        f,
        " SERVICE:{}",
        service_type_label(field(&data.bits, 0, 4))
    )?;
    write!(f, " MSG:{}", hex(&data.bits))
}

/// `UnknownDataMessage.toString()`.
pub fn fmt_unknown_data(data: &DataBurst, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if data.pattern.has_cach() {
        fmt_cach(&data.cach, f)?;
    } else {
        write!(f, "TS{}", data.timeslot)?;
    }
    if data.has_ras() {
        write!(f, " RAS:{}", data.reserved)?;
    }
    write!(f, " {} {}", data.slot_type(), hex(&data.bits))
}

/// `CACH.toString()`.
pub fn fmt_cach(cach: &Cach, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    use crate::protocol::dmr::fec::cach;
    if !cach.valid {
        f.write_str("[CRC-ERROR] ")?;
    }
    let lcss = ["[FL]", "[F-]", "[-L]", "[--]"][usize::from(cach.lcss & 3)];
    write!(
        f,
        "{} {lcss} TS:{}",
        if cach.busy { "BUSY" } else { "IDLE" },
        cach.timeslot
    )?;
    // The CACH message bits are not kept; rebuild TACT + payload for the checksum and hex.
    let mut message = [0u8; 24];
    message[0] = u8::from(cach.busy);
    message[1] = u8::from(cach.timeslot == 2);
    message[2] = (cach.lcss >> 1) & 1;
    message[3] = cach.lcss & 1;
    message[7..].copy_from_slice(&cach.payload);
    write!(
        f,
        " CRC:{} PAYLOAD:{} MSG:{} ",
        cach::get_crc_checksum(&message),
        hex(&cach.payload),
        hex(&message)
    )
}
