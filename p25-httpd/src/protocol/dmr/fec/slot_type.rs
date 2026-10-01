//! Slot type, Golay(20,8): port of SDRTrunk `module/decode/dmr/message/data/SlotType.java`
//! (and the `DataType` enum from `message/type/DataType.java`).
//!
//! The 20 bits are padded with 4 leading zeros to use the Golay(24,12) decoder,
//! as SDRTrunk does. Unlike SDRTrunk (whose `errorCount < 3` test always
//! passes), a correction that lands on a pad bit or a detected 4-bit error
//! makes the slot type invalid.

use super::{get_int, golay24};

/// Burst positions of the 20 slot type bits (10 either side of the sync).
pub const MESSAGE_INDEXES: [usize; 20] = [
    122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 180, 181, 182, 183, 184, 185, 186, 187, 188,
    189,
];

/// DMR data type (slot type bits 4..8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    PiHeader,
    VoiceHeader,
    /// Terminator with LC.
    Tlc,
    Csbk,
    MbcHeader,
    MbcBlock,
    DataHeader,
    Rate1Of2Data,
    Rate3Of4Data,
    SlotIdle,
    Rate1Data,
    UsbData,
    MbcEncHeader,
    DataEncHeader,
    ChannelControlEncHeader,
    Reserved15,
}

impl DataType {
    /// Data type for a 4-bit value. Ports `DataType.fromValue()`.
    pub fn from_value(value: u8) -> DataType {
        const TYPES: [DataType; 16] = [
            DataType::PiHeader,
            DataType::VoiceHeader,
            DataType::Tlc,
            DataType::Csbk,
            DataType::MbcHeader,
            DataType::MbcBlock,
            DataType::DataHeader,
            DataType::Rate1Of2Data,
            DataType::Rate3Of4Data,
            DataType::SlotIdle,
            DataType::Rate1Data,
            DataType::UsbData,
            DataType::MbcEncHeader,
            DataType::DataEncHeader,
            DataType::ChannelControlEncHeader,
            DataType::Reserved15,
        ];
        TYPES[usize::from(value & 0xF)]
    }
}

/// Decoded slot type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotType {
    /// Colour code, 0..15.
    pub color_code: u8,
    pub data_type: DataType,
    /// Bits corrected by Golay.
    pub corrected: u32,
}

impl SlotType {
    /// Decodes the slot type of a 288-bit burst. Ports `SlotType.getSlotType()`.
    pub fn get_slot_type(burst: &[u8]) -> Option<SlotType> {
        let mut bits = [0u8; 20];
        for (x, bit) in bits.iter_mut().enumerate() {
            *bit = burst[MESSAGE_INDEXES[x]];
        }
        Self::decode(&bits)
    }

    /// Decodes the 20 slot type bits (CC, DT, 12 parity), `None` if uncorrectable.
    pub fn decode(bits: &[u8; 20]) -> Option<SlotType> {
        let mut message = [0u8; 24];
        message[4..].copy_from_slice(bits);
        let corrected = golay24::check_and_correct(&mut message, 0)?;
        if message[..4] != [0, 0, 0, 0] {
            return None;
        }
        Some(SlotType {
            color_code: get_int(&message[4..8]) as u8,
            data_type: DataType::from_value(get_int(&message[8..12]) as u8),
            corrected,
        })
    }
}

#[cfg(test)]
#[path = "slot_type_tests.rs"]
mod tests;
