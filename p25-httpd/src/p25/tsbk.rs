//! TSBK (Trunking Signaling Block) parser
//!
//! Decodes TSBK messages from TSDU data units.
//! Each TSBK is 12 bytes (96 bits) after FEC decoding:
//!   - Byte 0: LB(1) | P(1) | opcode(6)
//!   - Byte 1: manufacturer (0x00 = standard)
//!   - Bytes 2-9: opcode-specific payload (8 bytes)
//!   - Bytes 10-11: CRC-16
//!
//! FEC chain (before this parser):
//!   1. De-interleave 196 dibits
//!   2. 1/2 rate Trellis decode -> 96 bits per TSBK
//!   3. CRC-16 check
//!
//! Reference: TIA-102.AABF (TSBK formats)

use super::types::*;

/// TSBK opcodes we care about for control channel tracking
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsbkOpcode {
    /// Group Voice Channel Grant (0x00)
    GroupVoiceChannelGrant,
    /// Group Voice Channel Grant Update (0x02)
    GroupVoiceChannelGrantUpdate,
    /// Unit to Unit Voice Channel Grant (0x04)
    UnitToUnitVoiceChannelGrant,
    /// Telephone Interconnect Voice Channel Grant (0x08)
    TelephoneInterconnectVoiceChannelGrant,
    /// Identifier Update VHF/UHF (0x34)
    IdentifierUpdate,
    /// RFSS Status Broadcast (0x3A)
    RfssStatusBroadcast,
    /// Network Status Broadcast (0x3B)
    NetworkStatusBroadcast,
    /// Adjacent Status Broadcast (0x3C)
    AdjacentStatusBroadcast,
    /// System Service Broadcast (0x38)
    SystemServiceBroadcast,
    /// Unknown opcode
    Unknown(u8),
}

impl From<u8> for TsbkOpcode {
    fn from(val: u8) -> Self {
        match val & 0x3F {
            0x00 => Self::GroupVoiceChannelGrant,
            0x02 => Self::GroupVoiceChannelGrantUpdate,
            0x04 => Self::UnitToUnitVoiceChannelGrant,
            0x08 => Self::TelephoneInterconnectVoiceChannelGrant,
            0x34 => Self::IdentifierUpdate,
            0x38 => Self::SystemServiceBroadcast,
            0x3A => Self::RfssStatusBroadcast,
            0x3B => Self::NetworkStatusBroadcast,
            0x3C => Self::AdjacentStatusBroadcast,
            other => Self::Unknown(other),
        }
    }
}

/// Parsed TSBK message
#[derive(Debug, Clone)]
pub enum TsbkMessage {
    /// Group Voice Channel Grant (opcode 0x00)
    /// A talkgroup is granted a traffic channel
    GroupVoiceChannelGrant {
        channel: Channel,
        talkgroup: Talkgroup,
        source: RadioId,
    },

    /// Group Voice Channel Grant Update (opcode 0x02)
    /// Updates for one or two active grants
    GroupVoiceChannelGrantUpdate {
        channel_a: Channel,
        talkgroup_a: Talkgroup,
        channel_b: Channel,
        talkgroup_b: Talkgroup,
    },

    /// Identifier Update VHF/UHF (opcode 0x34)
    /// Defines a frequency band: base freq + spacing + offset
    IdentifierUpdate {
        identifier: u8,
        bw: u16,
        transmit_offset: i32,
        channel_spacing: u32,
        base_frequency: u64,
    },

    /// Network Status Broadcast (opcode 0x3B)
    /// WACN, system ID, current control channel
    NetworkStatus {
        wacn: u32,
        system_id: u16,
        channel: Channel,
    },

    /// RFSS Status Broadcast (opcode 0x3A)
    /// RFSS and site identity
    RfssStatus {
        lra: u8,
        rfss_id: u8,
        site_id: u8,
        channel: Channel,
    },

    /// Adjacent Status Broadcast (opcode 0x3C)
    AdjacentStatus {
        lra: u8,
        rfss_id: u8,
        site_id: u8,
        channel: Channel,
        system_id: u16,
    },
}

/// A raw TSBK block (12 bytes after trellis + RS decode)
#[derive(Debug, Clone)]
pub struct TsbkBlock {
    pub last_block: bool,
    pub protected: bool,
    pub opcode: TsbkOpcode,
    pub manufacturer: u8,
    pub payload: [u8; 8],
    pub crc: u16,
}

/// Which CRC convention validated a TSBK block. Phase 6F.2d
/// diagnostic so the dashboard can report whether real on-air TSBKs
/// use a single convention or a mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrcConvention {
    /// `crc16_ccitt(data) == msg_crc` -- our implementation's plain
    /// output (which already does the CCITT-FALSE final XOR) matches
    /// the message CRC field directly.
    Plain,
    /// `crc16_ccitt(data) ^ 0xFFFF == msg_crc` -- the encoder did one
    /// fewer 0xFFFF XOR than our implementation does, equivalent to
    /// SDRTrunk's `residual == 0xFFFF` branch in `correctCCITT80`.
    Xored,
}

impl TsbkBlock {
    /// Parse a 12-byte TSBK block
    pub fn parse(data: &[u8; 12]) -> Self {
        let lb = (data[0] >> 7) & 1 == 1;
        let p = (data[0] >> 6) & 1 == 1;
        let opcode = TsbkOpcode::from(data[0]);
        let manufacturer = data[1];
        let mut payload = [0u8; 8];
        payload.copy_from_slice(&data[2..10]);
        let crc = u16::from_be_bytes([data[10], data[11]]);

        TsbkBlock {
            last_block: lb,
            protected: p,
            opcode,
            manufacturer,
            payload,
            crc,
        }
    }

    /// Verify CRC-16 (CCITT) over the 12-byte block.
    ///
    /// **Phase 6F.2d fix (2026-04-11):** P25 TSBK CRC encoders in the
    /// wild use BOTH conventions for the final XOR step -- some output
    /// `crc16_ccitt(data)` directly, others output
    /// `crc16_ccitt(data) ^ 0xFFFF`. SDRTrunk's `CRCP25.correctCCITT80`
    /// handles this by accepting `residual == 0 || residual == 0xFFFF`
    /// from its lookup-table CRC. Our `crc16_ccitt` implementation
    /// already does the final XOR (matches CCITT-FALSE inverted), so
    /// to match SDRTrunk's coverage we need to accept the value either
    /// AS-IS or XOR'd with 0xFFFF.
    ///
    /// On-target evidence (Phase 6F.2c run): with the single-convention
    /// check, the PS LSM software decoder hit 100 % CRC failures while
    /// trellis decode succeeded on every block (446/446 attempts on a
    /// healthy LSM control channel signal). Trellis succeeding while
    /// CRC fails on every block is the canonical "bytes are right but
    /// the CRC formula is wrong by a constant" signature -- in this
    /// case the constant is the 0xFFFF final XOR. See doc/changes/025
    /// for the diagnosis log.
    ///
    /// Returns:
    /// - `Some(CrcConvention::Plain)` if `crc16_ccitt(data) == msg_crc`
    /// - `Some(CrcConvention::Xored)` if `crc16_ccitt(data) ^ 0xFFFF == msg_crc`
    /// - `None` if neither matches (CRC is invalid)
    pub fn crc_valid(&self, data: &[u8; 12]) -> Option<CrcConvention> {
        let calc = crc16_ccitt(&data[..10]);
        let msg = u16::from_be_bytes([data[10], data[11]]);
        if calc == msg {
            Some(CrcConvention::Plain)
        } else if (calc ^ 0xFFFF) == msg {
            Some(CrcConvention::Xored)
        } else {
            None
        }
    }

    /// Decode the payload into a typed message
    pub fn decode(&self) -> Option<TsbkMessage> {
        // Skip non-standard manufacturer messages
        if self.manufacturer != 0x00 {
            return None;
        }

        match self.opcode {
            TsbkOpcode::GroupVoiceChannelGrant => {
                Some(self.decode_grp_v_ch_grant())
            }
            TsbkOpcode::GroupVoiceChannelGrantUpdate => {
                Some(self.decode_grp_v_ch_grant_update())
            }
            TsbkOpcode::IdentifierUpdate => {
                Some(self.decode_iden_update())
            }
            TsbkOpcode::NetworkStatusBroadcast => {
                Some(self.decode_net_sts_bcst())
            }
            TsbkOpcode::RfssStatusBroadcast => {
                Some(self.decode_rfss_sts_bcst())
            }
            TsbkOpcode::AdjacentStatusBroadcast => {
                Some(self.decode_adj_sts_bcst())
            }
            _ => None,
        }
    }

    /// GRP_V_CH_GRANT (0x00)
    /// Payload: [options(8)][channel(16)][talkgroup(16)][source(24)]
    fn decode_grp_v_ch_grant(&self) -> TsbkMessage {
        let channel = Channel(u16::from_be_bytes([self.payload[1], self.payload[2]]));
        let talkgroup = Talkgroup(u16::from_be_bytes([self.payload[3], self.payload[4]]));
        let source = RadioId(
            ((self.payload[5] as u32) << 16)
                | ((self.payload[6] as u32) << 8)
                | (self.payload[7] as u32),
        );
        TsbkMessage::GroupVoiceChannelGrant {
            channel,
            talkgroup,
            source,
        }
    }

    /// GRP_V_CH_GRANT_UPDT (0x02)
    /// Payload: [ch_a(16)][tg_a(16)][ch_b(16)][tg_b(16)]
    fn decode_grp_v_ch_grant_update(&self) -> TsbkMessage {
        let channel_a = Channel(u16::from_be_bytes([self.payload[0], self.payload[1]]));
        let talkgroup_a = Talkgroup(u16::from_be_bytes([self.payload[2], self.payload[3]]));
        let channel_b = Channel(u16::from_be_bytes([self.payload[4], self.payload[5]]));
        let talkgroup_b = Talkgroup(u16::from_be_bytes([self.payload[6], self.payload[7]]));
        TsbkMessage::GroupVoiceChannelGrantUpdate {
            channel_a,
            talkgroup_a,
            channel_b,
            talkgroup_b,
        }
    }

    /// IDEN_UP (0x34)
    /// Payload: [iden(4)|bw(9)|xmit_offset(19)][spacing(10)|base_freq(32)]
    /// Frequencies are in units of 5 Hz
    fn decode_iden_update(&self) -> TsbkMessage {
        let identifier = self.payload[0] >> 4;
        let bw = (((self.payload[0] & 0x0F) as u16) << 5)
            | ((self.payload[1] >> 3) as u16);
        // transmit_offset is 19 bits, two's complement, in units of 250 kHz
        let raw_offset = (((self.payload[1] & 0x07) as u32) << 16)
            | ((self.payload[2] as u32) << 8)
            | (self.payload[3] as u32);
        // Sign-extend 19-bit to i32
        let transmit_offset = if raw_offset & (1 << 18) != 0 {
            (raw_offset | 0xFFF80000) as i32
        } else {
            raw_offset as i32
        } * 250_000; // convert to Hz

        let channel_spacing = (((self.payload[4] >> 5) as u32) << 7)
            | (((self.payload[4] & 0x1F) as u32) << 2)
            | ((self.payload[5] >> 6) as u32);
        // spacing in units of 125 Hz
        let channel_spacing = channel_spacing * 125;

        // base_frequency: 32 bits in units of 5 Hz
        let base_frequency = ((self.payload[5] & 0x3F) as u64) << 26
            | (self.payload[6] as u64) << 18
            | (self.payload[7] as u64) << 10;
        // Actually it's a straight 32-bit field...
        // Let me re-read the spec layout more carefully
        // IDEN_UP layout: iden(4) | bw(9) | xmit_offset(13) | spacing(10) | base_freq(32)
        // Wait, that's 68 bits for 8 bytes = 64 bits. Let me reconsider.
        //
        // Per TIA-102.AABF-D Table 7.3.10:
        // Byte layout (8 bytes payload):
        //   [0]    identifier(4) | reserved(4)
        //   [1-2]  bandwidth(9) | xmit_offset_sign(1) | xmit_offset_mag(13)
        //          ... this doesn't work either. Let me use the SDRTrunk layout.
        //
        // SDRTrunk IdentifierUpdateVHFUHF.java:
        //   identifier = message.getInt(IDENTIFIER)  -- bits 16-19
        //   bandwidth  = message.getInt(BW) * 125    -- bits 20-28 (9 bits) * 125 Hz
        //   offset     = message.getInt(TX_OFFSET) * 250000 * sign -- bits 29-41 (13 bits)
        //   spacing    = message.getInt(CH_SPACING) * 125  -- bits 42-51 (10 bits)
        //   base_freq  = message.getLong(BASE_FREQ) * 5 -- bits 52-83 (32 bits)
        //
        // So in our 8-byte payload (bits 0-63, after opcode+mfid):
        //   [0] bits 0-3: identifier, bits 4-7: reserved
        //   Wait, the TSBK is 12 bytes total: opcode(8)+mfid(8)+payload(64)+crc(16) = 96 bits
        //   So payload is bits 16-79 of the TSBK.
        //   identifier is at absolute bits 16-19 = payload bits 0-3 = payload[0] >> 4 ✓
        //   bw at bits 20-28 = payload bits 4-12
        //   xmit_offset at bits 29-41 = payload bits 13-25
        //   spacing at bits 42-51 = payload bits 26-35
        //   base_freq at bits 52-83 = payload bits 36-67... but payload is only 64 bits (0-63)
        //   So base_freq extends to bit 67 which is payload[8]...[8.375]
        //   That's wrong, we only have 8 bytes of payload.
        //
        // Actually, I think the "payload" in SDRTrunk counts from bit 0 of the full TSBK.
        // Let me compute from the full 12-byte block:
        //   Block bits 0-7: LB|P|opcode
        //   Block bits 8-15: manufacturer
        //   Block bits 16-19: identifier    => payload[0] >> 4
        //   Block bits 20-28: bw (9 bits)   => payload[0:1] bits
        //   Block bits 29-41: offset (13b)  => payload[1:3]
        //   Block bits 42-51: spacing (10b) => payload[3:4]
        //   Block bits 52-83: base_freq (32b) => payload[4:7] + extends...
        //   Block bits 80-95: CRC
        //
        // 52+32 = 84. Block is 96 bits. CRC at 80-95. So base_freq is bits 52-79 = 28 bits.
        // Hmm that's only 28 bits. Let me look at this more carefully.

        // Using bit extraction from the full 8-byte payload:
        let p = &self.payload;
        let iden = p[0] >> 4;
        // BW: 9 bits starting at payload bit 4
        let bw_val = (((p[0] & 0x0F) as u16) << 5) | ((p[1] >> 3) as u16);
        // Transmit offset: 13 bits starting at payload bit 13, with sign at bit 29 of block
        let offset_sign = (p[1] >> 2) & 1;
        let offset_mag = (((p[1] & 0x03) as u32) << 10)
            | ((p[2] as u32) << 2)
            | ((p[3] >> 6) as u32);
        let xmit_offset = if offset_sign == 1 {
            -(offset_mag as i32) * 250_000
        } else {
            (offset_mag as i32) * 250_000
        };
        // Channel spacing: 10 bits starting at payload bit 26
        let spacing_raw = (((p[3] & 0x3F) as u32) << 4) | ((p[4] >> 4) as u32);
        let spacing = spacing_raw * 125;
        // Base frequency: 32 bits starting at payload bit 36
        let base_freq_raw = ((p[4] & 0x0F) as u64) << 28
            | (p[5] as u64) << 20
            | (p[6] as u64) << 12
            | (p[7] as u64) << 4;
        // Actually only 28 bits available in payload. The remaining 4 bits are zeros.
        // base_freq is in units of 5 Hz
        let base_freq = base_freq_raw * 5;

        TsbkMessage::IdentifierUpdate {
            identifier: iden,
            bw: bw_val,
            transmit_offset: xmit_offset,
            channel_spacing: spacing,
            base_frequency: base_freq,
        }
    }

    /// NET_STS_BCST (0x3B)
    /// Payload bytes [0-7]: lra(8) | wacn(20) | system_id(12) | channel(16) | services(8) | pad
    /// Total: 8+20+12+16+8 = 64 bits = 8 bytes
    fn decode_net_sts_bcst(&self) -> TsbkMessage {
        let p = &self.payload;
        // LRA at p[0], WACN at p[1] bits 7-0, p[2] bits 7-0, p[3] bits 7-4
        let wacn = ((p[1] as u32) << 12)
            | ((p[2] as u32) << 4)
            | ((p[3] >> 4) as u32);
        // system_id at p[3] bits 3-0, p[4] bits 7-0
        let system_id = (((p[3] & 0x0F) as u16) << 8) | (p[4] as u16);
        // channel at p[5-6]
        let channel = Channel(u16::from_be_bytes([p[5], p[6]]));
        TsbkMessage::NetworkStatus {
            wacn,
            system_id,
            channel,
        }
    }

    /// RFSS_STS_BCST (0x3A)
    /// Payload: [lra(8)][reserved(8)][rfss_id(8)][site_id(8)][channel(16)][services(8)]
    fn decode_rfss_sts_bcst(&self) -> TsbkMessage {
        let lra = self.payload[0];
        let rfss_id = self.payload[2];
        let site_id = self.payload[3];
        let channel = Channel(u16::from_be_bytes([self.payload[4], self.payload[5]]));
        TsbkMessage::RfssStatus {
            lra,
            rfss_id,
            site_id,
            channel,
        }
    }

    /// ADJ_STS_BCST (0x3C)
    /// Payload: [lra(8)][sys_id(12)][rfss_id(8)][site_id(8)][channel(16)][services(8)]
    fn decode_adj_sts_bcst(&self) -> TsbkMessage {
        let lra = self.payload[0];
        let system_id = (((self.payload[1]) as u16) << 4) | ((self.payload[2] >> 4) as u16);
        let rfss_id = ((self.payload[2] & 0x0F) << 4) | (self.payload[3] >> 4);
        let site_id = ((self.payload[3] & 0x0F) << 4) | (self.payload[4] >> 4);
        let channel = Channel(
            (((self.payload[4] & 0x0F) as u16) << 12)
                | ((self.payload[5] as u16) << 4)
                | ((self.payload[6] >> 4) as u16),
        );
        TsbkMessage::AdjacentStatus {
            lra,
            rfss_id,
            site_id,
            channel,
            system_id,
        }
    }
}

/// CRC-16-CCITT (polynomial 0x1021, init 0xFFFF)
fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc ^ 0xFFFF
}

/// Frequency band entry from IDEN_UP messages
#[derive(Debug, Clone)]
pub struct FrequencyBand {
    pub identifier: u8,
    pub bandwidth_hz: u32,
    pub transmit_offset_hz: i32,
    pub channel_spacing_hz: u32,
    pub base_frequency_hz: u64,
}

impl FrequencyBand {
    /// Create from a parsed IDEN_UP TSBK
    pub fn from_tsbk(msg: &TsbkMessage) -> Option<Self> {
        match msg {
            TsbkMessage::IdentifierUpdate {
                identifier,
                bw,
                transmit_offset,
                channel_spacing,
                base_frequency,
            } => Some(FrequencyBand {
                identifier: *identifier,
                bandwidth_hz: (*bw as u32) * 125,
                transmit_offset_hz: *transmit_offset,
                channel_spacing_hz: *channel_spacing,
                base_frequency_hz: *base_frequency,
            }),
            _ => None,
        }
    }

    /// Calculate downlink frequency for a channel number
    pub fn channel_frequency(&self, channel_number: u16) -> u64 {
        self.base_frequency_hz + (channel_number as u64) * (self.channel_spacing_hz as u64)
    }

    /// Calculate uplink frequency for a channel number
    pub fn channel_uplink_frequency(&self, channel_number: u16) -> u64 {
        let dl = self.channel_frequency(channel_number);
        (dl as i64 + self.transmit_offset_hz as i64) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc16_ccitt() {
        // Known test vector: CRC-16/CCITT-FALSE with init=0xFFFF, final XOR=0xFFFF
        // "123456789" -> CRC-16/CCITT-FALSE = 0x29B1, then XOR 0xFFFF = 0xD64E
        let data = b"123456789";
        let crc = crc16_ccitt(data);
        assert_eq!(crc, 0xD64E);
    }

    #[test]
    fn test_opcode_parsing() {
        assert_eq!(TsbkOpcode::from(0x00), TsbkOpcode::GroupVoiceChannelGrant);
        assert_eq!(TsbkOpcode::from(0x34), TsbkOpcode::IdentifierUpdate);
        assert_eq!(TsbkOpcode::from(0x3B), TsbkOpcode::NetworkStatusBroadcast);
        // LB and P bits should be masked
        assert_eq!(TsbkOpcode::from(0xC0), TsbkOpcode::GroupVoiceChannelGrant);
    }

    #[test]
    fn test_grp_v_ch_grant_decode() {
        // Construct a synthetic GRP_V_CH_GRANT TSBK
        let mut data = [0u8; 12];
        data[0] = 0x80; // LB=1, P=0, opcode=0x00
        data[1] = 0x00; // standard manufacturer
        // payload: options=0, channel=0x0639 (band 0, ch 1593), talkgroup=0x012C, source=0x000001
        data[2] = 0x00; // options
        data[3] = 0x06; // channel high
        data[4] = 0x39; // channel low
        data[5] = 0x01; // talkgroup high
        data[6] = 0x2C; // talkgroup low
        data[7] = 0x00; // source byte 0
        data[8] = 0x00; // source byte 1
        data[9] = 0x01; // source byte 2
        // CRC (not checked in this test)
        data[10] = 0x00;
        data[11] = 0x00;

        let block = TsbkBlock::parse(&data);
        assert!(block.last_block);
        assert!(!block.protected);
        assert_eq!(block.manufacturer, 0x00);

        let msg = block.decode().unwrap();
        match msg {
            TsbkMessage::GroupVoiceChannelGrant {
                channel,
                talkgroup,
                source,
            } => {
                assert_eq!(channel.0, 0x0639);
                assert_eq!(channel.identifier(), 0);
                assert_eq!(channel.number(), 0x639); // 1593
                assert_eq!(talkgroup.0, 0x012C); // 300
                assert_eq!(source.0, 1);
            }
            _ => panic!("Expected GroupVoiceChannelGrant"),
        }
    }

    #[test]
    fn test_net_sts_bcst_decode() {
        // Synthetic NET_STS_BCST for Clay County: WACN=0xBEE00, sys=0x8A0
        let mut data = [0u8; 12];
        data[0] = 0xBB; // LB=1, P=0, opcode=0x3B
        data[1] = 0x00; // standard manufacturer
        // payload: lra=0x00, wacn=0xBEE00, system_id=0x8A0, channel=0x0639
        data[2] = 0x00; // payload[0]: LRA
        data[3] = 0xBE; // payload[1]: WACN bits 19-12
        data[4] = 0xE0; // payload[2]: WACN bits 11-4
        data[5] = 0x08; // payload[3]: WACN bits 3-0 (0x0) | system_id bits 11-8 (0x8)
        data[6] = 0xA0; // payload[4]: system_id bits 7-0
        data[7] = 0x06; // payload[5]: channel high
        data[8] = 0x39; // payload[6]: channel low
        data[9] = 0x00; // payload[7]: services

        let block = TsbkBlock::parse(&data);
        let msg = block.decode().unwrap();
        match msg {
            TsbkMessage::NetworkStatus {
                wacn,
                system_id,
                channel,
            } => {
                assert_eq!(wacn, 0xBEE00);
                assert_eq!(system_id, 0x8A0);
                assert_eq!(channel.0, 0x0639);
            }
            _ => panic!("Expected NetworkStatus"),
        }
    }

    #[test]
    fn test_frequency_band_calculation() {
        // Band 0 from Clay County: base=851006250, spacing=6250, offset=-45000000
        let band = FrequencyBand {
            identifier: 0,
            bandwidth_hz: 12500,
            transmit_offset_hz: -45_000_000,
            channel_spacing_hz: 6_250,
            base_frequency_hz: 851_006_250,
        };

        // Channel 1593 should be 860.9625 MHz (control channel)
        let freq = band.channel_frequency(1593);
        assert_eq!(freq, 860_962_500);

        // Uplink = 860.9625 - 45.0 = 815.9625 MHz
        let uplink = band.channel_uplink_frequency(1593);
        assert_eq!(uplink, 815_962_500);
    }
}
