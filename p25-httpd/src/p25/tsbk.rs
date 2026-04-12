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

/// P25 service options byte bit constants. Used by all the
/// channel-grant TSBKs (`GroupVoiceChannelGrant`,
/// `UnitToUnitVoiceChannelGrant`, `TelephoneInterconnectVoiceChannelGrant`,
/// etc) to describe per-call attributes.
///
/// Verbatim from SDRTrunk upstream `ServiceOptions.java:27-30`
/// (`module/decode/p25/reference/ServiceOptions.java`):
///
/// ```text
/// EMERGENCY_FLAG  = 0x80   (bit 7)
/// ENCRYPTION_FLAG = 0x40   (bit 6)
/// DUPLEX          = 0x20   (bit 5) -- 1 = full, 0 = half
/// SESSION_MODE    = 0x10   (bit 4) -- 1 = packet, 0 = circuit
/// PRIORITY        = 0x07   (bits 0-2)
/// ```
///
/// Phase 7C: the `ENCRYPTION_FLAG` is what gates the Phase 7D
/// vocoder. We read it from the `GroupVoiceChannelGrant` TSBK so
/// the encrypted-call decision happens at the control channel
/// (before we retune the traffic DDC) instead of waiting for the
/// HDU on the voice channel. See
/// `reference_p25_encryption_flag_from_control_channel.md` memory.
pub mod service_options {
    pub const EMERGENCY_FLAG: u8 = 0x80;
    pub const ENCRYPTION_FLAG: u8 = 0x40;
    pub const DUPLEX_FLAG: u8 = 0x20;
    pub const SESSION_MODE_FLAG: u8 = 0x10;
    pub const PRIORITY_MASK: u8 = 0x07;

    /// Returns true if the service options byte has the encryption
    /// bit set.
    #[inline]
    pub fn is_encrypted(opts: u8) -> bool {
        opts & ENCRYPTION_FLAG != 0
    }

    /// Returns true if the service options byte has the emergency
    /// bit set.
    #[inline]
    pub fn is_emergency(opts: u8) -> bool {
        opts & EMERGENCY_FLAG != 0
    }
}

/// TSBK opcodes we care about for control channel tracking
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsbkOpcode {
    /// Group Voice Channel Grant (0x00)
    GroupVoiceChannelGrant,
    /// Group Voice Channel Grant Update (0x02)
    GroupVoiceChannelGrantUpdate,
    /// Unit to Unit Voice Channel Grant (0x04)
    UnitToUnitVoiceChannelGrant,
    /// Unit to Unit Answer Request (0x05) -- private call paging.
    /// Phase 6F.11.
    UnitToUnitAnswerRequest,
    /// Telephone Interconnect Voice Channel Grant (0x08)
    TelephoneInterconnectVoiceChannelGrant,
    /// Telephone Interconnect Voice Channel Grant Update (0x09).
    /// Phase 6F.11.
    TelephoneInterconnectVoiceChannelGrantUpdate,
    /// SNDCP Data Channel Announcement Explicit (0x16) -- which
    /// downlink/uplink channel carries SNDCP packet data services.
    /// Phase 6F.11.
    SndcpDataChannelAnnouncementExplicit,
    /// TDMA Synchronization Broadcast (0x30) -- system date/time +
    /// microslot rollover info. Phase 6F.11.
    TdmaSyncBroadcast,
    /// Identifier Update TDMA (0x33) -- TDMA frequency band, 4-bit
    /// channel-type field instead of bandwidth, 13-bit transmit offset
    /// at bits 25-37, otherwise same layout as VUHF.
    IdentifierUpdateTdma,
    /// Identifier Update VHF/UHF (0x34) -- VUHF frequency band,
    /// 4-bit bandwidth, 13-bit transmit offset at bits 25-37.
    IdentifierUpdateVuhf,
    /// System Service Broadcast (0x38)
    SystemServiceBroadcast,
    /// Secondary Control Channel Broadcast (0x39) -- backup CCH A/B
    /// channels for trunking failover. Phase 6F.11.
    SecondaryControlChannelBroadcast,
    /// Identifier Update standard FDMA (0x3D) -- THE common one on
    /// Clay County and most P25 sites. 9-bit bandwidth, 8-bit transmit
    /// offset at bits 30-37. NOT the same as opcode 0x34 (VUHF) which
    /// has a different field layout.
    IdentifierUpdate,
    /// RFSS Status Broadcast (0x3A)
    RfssStatusBroadcast,
    /// Network Status Broadcast (0x3B)
    NetworkStatusBroadcast,
    /// Adjacent Status Broadcast (0x3C)
    AdjacentStatusBroadcast,
    /// Unknown opcode
    Unknown(u8),
}

impl From<u8> for TsbkOpcode {
    fn from(val: u8) -> Self {
        match val & 0x3F {
            0x00 => Self::GroupVoiceChannelGrant,
            0x02 => Self::GroupVoiceChannelGrantUpdate,
            0x04 => Self::UnitToUnitVoiceChannelGrant,
            0x05 => Self::UnitToUnitAnswerRequest,
            0x08 => Self::TelephoneInterconnectVoiceChannelGrant,
            0x09 => Self::TelephoneInterconnectVoiceChannelGrantUpdate,
            0x16 => Self::SndcpDataChannelAnnouncementExplicit,
            0x30 => Self::TdmaSyncBroadcast,
            0x33 => Self::IdentifierUpdateTdma,
            0x34 => Self::IdentifierUpdateVuhf,
            0x38 => Self::SystemServiceBroadcast,
            0x39 => Self::SecondaryControlChannelBroadcast,
            0x3A => Self::RfssStatusBroadcast,
            0x3B => Self::NetworkStatusBroadcast,
            0x3C => Self::AdjacentStatusBroadcast,
            0x3D => Self::IdentifierUpdate,
            other => Self::Unknown(other),
        }
    }
}

/// Parsed TSBK message
#[derive(Debug, Clone)]
pub enum TsbkMessage {
    /// Group Voice Channel Grant (opcode 0x00)
    /// A talkgroup is granted a traffic channel.
    ///
    /// `service_options` is the raw 8-bit service options byte
    /// (SDRTrunk `GroupVoiceChannelGrant.java` SERVICE_OPTIONS at
    /// bit positions 16-23). Decode bits via `ServiceOptions::*`
    /// helpers in this module. Phase 7C uses the encryption bit
    /// (mask `0x40`) to gate the Phase 7D vocoder so we don't try
    /// to vocode encrypted IMBE frames.
    GroupVoiceChannelGrant {
        channel: Channel,
        talkgroup: Talkgroup,
        source: RadioId,
        service_options: u8,
    },

    /// Group Voice Channel Grant Update (opcode 0x02)
    /// Updates for one or two active grants. **Does NOT carry
    /// service options** -- the GVCG_UPDATE TSBK packs two
    /// (channel, talkgroup) pairs tightly with no service-options
    /// bytes. The encryption flag must be inherited from the
    /// original `GroupVoiceChannelGrant` for the same TG (the
    /// grant store does this in `control_channel.rs`).
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

    /// Secondary Control Channel Broadcast (opcode 0x39) -- backup
    /// CCH channels A and B for the same RFSS/site. P25 trunking
    /// failover; SDRTrunk's `SecondaryControlChannelBroadcast.java`.
    /// Phase 6F.11.
    SecondaryControlChannelBroadcast {
        rfss_id: u8,
        site_id: u8,
        channel_a: Channel,
        channel_b: Channel,
    },

    /// SNDCP Data Channel Announcement Explicit (opcode 0x16) --
    /// downlink + uplink channels carrying SNDCP packet-data services
    /// on this site. Phase 6F.11.
    SndcpDataChannelAnnouncementExplicit {
        autonomous_access: bool,
        requested_access: bool,
        downlink_channel: Channel,
        uplink_channel: Channel,
        data_access_control: u16,
    },

    /// TDMA Synchronization Broadcast (opcode 0x30) -- system
    /// date/time + microslot rollover. Phase 6F.11. We expose just
    /// enough fields to display "system clock" status; full ISO 8601
    /// formatting is left to the dashboard if/when it cares.
    TdmaSyncBroadcast {
        time_locked: bool,
        year: u16,
        month: u8,
        day: u8,
        hours: u8,
        minutes: u8,
        micro_slots: u16,
    },

    /// Telephone Interconnect Voice Channel Grant Update (opcode
    /// 0x09). A non-talkgroup grant -- "any address" is the unit ID
    /// being patched to a phone number. Phase 6F.11.
    TelephoneInterconnectVoiceChannelGrantUpdate {
        channel: Channel,
        call_timer_secs: u16,
        unit_id: RadioId,
    },

    /// Unit-to-Unit Answer Request (opcode 0x05) -- private call
    /// paging from `source` to `target`. No channel grant; the
    /// dispatcher just routes paging info onto the activity feed.
    /// Phase 6F.11.
    UnitToUnitAnswerRequest {
        target: RadioId,
        source: RadioId,
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

/// **SDRTrunk `CRCP25.CCITT_80_CHECKSUMS`** -- per-bit XOR table for
/// the CRC-16/CCITT used to protect P25 80-bit (10-byte) TSBK
/// payloads. Generated by SDRTrunk's `CRCUtil.generate(80, 16,
/// 0x11021, 0xFFFF, true)` and copied verbatim. The first 80 entries
/// are the residue of `x^k` for each data-bit position; the last 16
/// entries (powers of 2) are CRC-self-correction look-ups.
///
/// Phase 6F.2j: replaces our previous standard byte-wise
/// `crc16_ccitt` implementation, which had the wrong bit reflection
/// convention for SDRTrunk's reference encoder. Cross-checked
/// against captured on-air bytes that decoded with `metric=0` after
/// the trellis deinterleave fix -- this table-based CRC validates
/// while the byte-wise CRC didn't.
const CCITT_80_CHECKSUMS: [u16; 96] = [
    0x1BCB, 0x8DE5, 0xC6F2, 0x6B69, 0xB5B4, 0x52CA, 0x2175, 0x90BA, 0x404D,
    0xA026, 0x5803, 0xAC01, 0xD600, 0x6310, 0x3998, 0x14DC, 0x027E, 0x092F,
    0x8497, 0xC24B, 0xE125, 0xF092, 0x7059, 0xB82C, 0x5406, 0x2213, 0x9109,
    0xC884, 0x6C52, 0x3E39, 0x9F1C, 0x479E, 0x2BDF, 0x95EF, 0xCAF7, 0xE57B,
    0xF2BD, 0xF95E, 0x74BF, 0xBA5F, 0xDD2F, 0xEE97, 0xF74B, 0xFBA5, 0xFDD2,
    0x76F9, 0xBB7C, 0x55AE, 0x22C7, 0x9163, 0xC8B1, 0xE458, 0x7A3C, 0x350E,
    0x1297, 0x894B, 0xC4A5, 0xE252, 0x7939, 0xBC9C, 0x565E, 0x233F, 0x919F,
    0xC8CF, 0xE467, 0xF233, 0xF919, 0xFC8C, 0x7656, 0x333B, 0x999D, 0xCCCE,
    0x6E77, 0xB73B, 0xDB9D, 0xEDCE, 0x7EF7, 0xBF7B, 0xDFBD, 0xEFDE, 0x0001,
    0x0002, 0x0004, 0x0008, 0x0010, 0x0020, 0x0040, 0x0080, 0x0100, 0x0200,
    0x0400, 0x0800, 0x1000, 0x2000, 0x4000, 0x8000,
];

/// Compute SDRTrunk's CCITT_80 CRC over the first 80 bits (10 bytes,
/// MSB-first) of `data`. Returns the running calculator value;
/// caller XORs with the message CRC field to get the residual.
pub fn ccitt80_crc(data: &[u8]) -> u16 {
    let mut calc: u16 = 0xFFFF;
    for byte_idx in 0..10 {
        let b = data[byte_idx];
        for bit_idx in 0..8 {
            // bit 0 of message = MSB of byte 0
            let bit_pos = byte_idx * 8 + bit_idx;
            if (b >> (7 - bit_idx)) & 1 != 0 {
                calc ^= CCITT_80_CHECKSUMS[bit_pos];
            }
        }
    }
    calc
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
        // Phase 6F.2j: switched from byte-wise crc16_ccitt to SDRTrunk's
        // table-based CCITT_80. The byte-wise CRC had the wrong bit
        // reflection convention and consistently produced different
        // values from the encoder for any non-trivial input. The
        // table-based approach mirrors `CRCP25.correctCCITT80` exactly:
        // residual = calc XOR msg_crc; valid if residual is 0 or 0xFFFF.
        let calc = ccitt80_crc(data);
        let msg = u16::from_be_bytes([data[10], data[11]]);
        let residual = calc ^ msg;
        if residual == 0 {
            Some(CrcConvention::Plain)
        } else if residual == 0xFFFF {
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
                Some(self.decode_iden_update_fdma())
            }
            TsbkOpcode::IdentifierUpdateVuhf => {
                Some(self.decode_iden_update_vuhf())
            }
            TsbkOpcode::IdentifierUpdateTdma => {
                Some(self.decode_iden_update_tdma())
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
            TsbkOpcode::SecondaryControlChannelBroadcast => {
                Some(self.decode_secondary_cch_bcst())
            }
            TsbkOpcode::SndcpDataChannelAnnouncementExplicit => {
                Some(self.decode_sndcp_dch_ann_ex())
            }
            TsbkOpcode::TdmaSyncBroadcast => {
                Some(self.decode_tdma_sync_bcst())
            }
            TsbkOpcode::TelephoneInterconnectVoiceChannelGrantUpdate => {
                Some(self.decode_tele_int_v_ch_grant_update())
            }
            TsbkOpcode::UnitToUnitAnswerRequest => {
                Some(self.decode_uu_ans_req())
            }
            _ => None,
        }
    }

    /// GRP_V_CH_GRANT (0x00)
    /// Payload: [options(8)][channel(16)][talkgroup(16)][source(24)]
    ///
    /// Phase 7C (2026-04-11): now extracts the service options byte
    /// at `payload[0]`. SDRTrunk `GroupVoiceChannelGrant.java` SERVICE_OPTIONS
    /// = {16, 17, 18, 19, 20, 21, 22, 23} (bits 16-23 of the TSBK,
    /// which is `payload[0]` because the maia-sdr `payload` array
    /// skips the 16-bit TSBK header). The encryption flag is bit 6
    /// (mask `0x40`); the Phase 7D vocoder reads this from the
    /// grant store to skip vocoding encrypted calls without
    /// having to parse the HDU on the voice channel.
    fn decode_grp_v_ch_grant(&self) -> TsbkMessage {
        let service_options = self.payload[0];
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
            service_options,
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

    /// Helper: read `n` bits from the 12-byte TSBK starting at absolute
    /// bit position `start` (where bit 0 is the MSB of byte 0). Returns
    /// the bits packed into a u64, MSB-first. Used by all the SDRTrunk-
    /// style absolute-bit-position bit-field extractors below.
    fn bits(&self, data: &[u8; 12], start: usize, n: usize) -> u64 {
        let mut out = 0u64;
        for i in 0..n {
            let pos = start + i;
            let byte = data[pos / 8];
            let bit = (byte >> (7 - (pos % 8))) & 1;
            out = (out << 1) | bit as u64;
        }
        out
    }

    /// Sign-extend an n-bit two's-complement value into i32 with sign
    /// bit at the MSB of `n`.
    fn sign_extend(val: u64, n: usize) -> i32 {
        let sign = (val >> (n - 1)) & 1;
        if sign == 1 {
            (val | (!0u64 << n)) as i32
        } else {
            val as i32
        }
    }

    /// IDEN_UPDATE (opcode 0x3D) -- standard FDMA frequency band entry.
    /// SDRTrunk's `FrequencyBandUpdate.java` absolute-bit-position
    /// layout (bit 0 = MSB of byte 0):
    ///
    /// | Field             | Bits  | Width |
    /// |-------------------|-------|-------|
    /// | identifier        | 16-19 | 4     |
    /// | bandwidth (×125)  | 20-28 | 9     |
    /// | offset sign       | 29    | 1     |
    /// | transmit offset   | 30-37 | 8     |
    /// | channel spacing   | 38-47 | 10    |
    /// | base frequency    | 48-79 | 32    |
    ///
    /// Note that this is DIFFERENT from VUHF (0x34) which has a 4-bit
    /// bandwidth + 13-bit offset starting at bit 25. SDRTrunk maps both
    /// to separate Java classes; we route them to separate decode_*
    /// functions but emit the same `TsbkMessage::IdentifierUpdate`
    /// variant since the downstream `FrequencyBand` consumer cares
    /// about the same fields.
    ///
    /// Phase 6F.4 fix: this opcode (0x3D) is the one Clay County
    /// actually broadcasts. Until Phase 6F.4 we mapped 0x34 to
    /// `IdentifierUpdate` and never decoded 0x3D, which is why
    /// `bands_known` stayed at 0 even after the multi-block 6F.3 work
    /// landed. Verified against the SDRTrunk reference recording from
    /// 2026-04-11: every `TSBK1/2/3 IDEN_UPDATE` line in the log uses
    /// FDMA layout, never VUHF.
    fn decode_iden_update_fdma(&self) -> TsbkMessage {
        // We need the full 12-byte TSBK to bit-extract from absolute
        // positions, but we only stored payload[0..8] (= bits 16-79).
        // Reconstruct a synthetic 12-byte buffer with zeros for bytes
        // 0/1/10/11 (we don't read them) and the payload in the middle.
        let mut full = [0u8; 12];
        full[2..10].copy_from_slice(&self.payload);

        let identifier = self.bits(&full, 16, 4) as u8;
        let bw_raw = self.bits(&full, 20, 9) as u16;
        let bw = bw_raw; // SDRTrunk multiplies by 125 in the getter; we
                        // emit raw and let FrequencyBand do that.
        let offset_sign = self.bits(&full, 29, 1);
        let offset_mag = self.bits(&full, 30, 8);
        // SDRTrunk: `if (!sign) offset *= -1` -- sign=1 means POSITIVE
        let mut xmit_offset = (offset_mag as i32) * 250_000;
        if offset_sign == 0 {
            xmit_offset = -xmit_offset;
        }
        let spacing = self.bits(&full, 38, 10) as u32 * 125;
        let base_frequency = self.bits(&full, 48, 32) * 5;

        TsbkMessage::IdentifierUpdate {
            identifier,
            bw,
            transmit_offset: xmit_offset,
            channel_spacing: spacing,
            base_frequency,
        }
    }

    /// IDEN_UPDATE_VHF_UHF (opcode 0x34) -- VUHF frequency band.
    /// SDRTrunk `FrequencyBandUpdateVUHF.java`:
    ///
    /// | Field             | Bits  | Width |
    /// |-------------------|-------|-------|
    /// | identifier        | 16-19 | 4     |
    /// | bandwidth (×125)  | 20-23 | 4     |
    /// | offset sign       | 24    | 1     |
    /// | transmit offset   | 25-37 | 13    |
    /// | channel spacing   | 38-47 | 10    |
    /// | base frequency    | 48-79 | 32    |
    fn decode_iden_update_vuhf(&self) -> TsbkMessage {
        let mut full = [0u8; 12];
        full[2..10].copy_from_slice(&self.payload);

        let identifier = self.bits(&full, 16, 4) as u8;
        let bw = self.bits(&full, 20, 4) as u16;
        let offset_sign = self.bits(&full, 24, 1);
        let offset_mag = self.bits(&full, 25, 13);
        let mut xmit_offset = (offset_mag as i32) * 250_000;
        if offset_sign == 0 {
            xmit_offset = -xmit_offset;
        }
        let spacing = self.bits(&full, 38, 10) as u32 * 125;
        let base_frequency = self.bits(&full, 48, 32) * 5;

        TsbkMessage::IdentifierUpdate {
            identifier,
            bw,
            transmit_offset: xmit_offset,
            channel_spacing: spacing,
            base_frequency,
        }
    }

    /// IDEN_UPDATE_TDMA (opcode 0x33). SDRTrunk
    /// `FrequencyBandUpdateTDMA.java`:
    ///
    /// | Field             | Bits  | Width |
    /// |-------------------|-------|-------|
    /// | identifier        | 16-19 | 4     |
    /// | channel type      | 20-23 | 4     |
    /// | offset sign       | 24    | 1     |
    /// | transmit offset   | 25-37 | 13    |
    /// | channel spacing   | 38-47 | 10    |
    /// | base frequency    | 48-79 | 32    |
    ///
    /// We expose the channel type via the `bw` field of
    /// `IdentifierUpdate` for now (SDRTrunk stores TDMA bandwidth in a
    /// separate enum mapped from `channel type`). Downstream the
    /// `FrequencyBand` consumer treats it as bandwidth which is wrong
    /// for TDMA -- not a problem for control-channel tracking which
    /// only uses base_frequency + spacing.
    ///
    /// **Phase 6F.5 fix:** TDMA offset is `mag * channel_spacing`, NOT
    /// `mag * 250000` like FDMA/VUHF. SDRTrunk's
    /// `FrequencyBandUpdateTDMA.getTransmitOffset()`:
    ///
    /// ```java
    /// long offset = getMessage().getLong(TRANSMIT_OFFSET) * getChannelSpacing();
    /// ```
    ///
    /// Until 6F.5 we used `* 250_000` and the resulting offset was
    /// wrong by a factor of `250000 / channel_spacing` -- on the Clay
    /// County 12.5 kHz TDMA bands that's `250000 / 12500 = 20`, so
    /// band 5 reported -780 MHz instead of -39 MHz. Verified
    /// against the SDRTrunk reference recording.
    fn decode_iden_update_tdma(&self) -> TsbkMessage {
        let mut full = [0u8; 12];
        full[2..10].copy_from_slice(&self.payload);

        let identifier = self.bits(&full, 16, 4) as u8;
        let channel_type = self.bits(&full, 20, 4) as u16;
        let offset_sign = self.bits(&full, 24, 1);
        let offset_mag = self.bits(&full, 25, 13);
        let spacing = self.bits(&full, 38, 10) as u32 * 125;
        // TDMA-specific: offset is in units of channel_spacing, NOT
        // 250 kHz. See doc above.
        let mut xmit_offset = (offset_mag as i32) * (spacing as i32);
        if offset_sign == 0 {
            xmit_offset = -xmit_offset;
        }
        let base_frequency = self.bits(&full, 48, 32) * 5;

        TsbkMessage::IdentifierUpdate {
            identifier,
            bw: channel_type, // see doc above
            transmit_offset: xmit_offset,
            channel_spacing: spacing,
            base_frequency,
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

    /// RFSS_STS_BCST (0x3A). SDRTrunk `RFSSStatusBroadcast.java` bit
    /// layout (absolute bit positions in the 12-byte TSBK):
    ///
    /// | Field            | Bits  | Width |
    /// |------------------|-------|-------|
    /// | LRA              | 16-23 | 8     |
    /// | active conn      | 27    | 1     |
    /// | system           | 28-39 | 12    |
    /// | RFSS             | 40-47 | 8     |
    /// | site             | 48-55 | 8     |
    /// | freq band        | 56-59 | 4     |
    /// | channel number   | 60-71 | 12    |
    /// | system service   | 72-79 | 8     |
    ///
    /// Phase 6F.4 fix: until 6F.4 we read RFSS from `payload[2]`
    /// (= bits 32-39, which is actually the LOW byte of the SYSTEM
    /// field). On Clay County `system_id = 0x8A0`, low byte = 0xA0 =
    /// 160 -- exactly the wrong value we were reporting via
    /// `/api/system`. Same off-by-one shift on site/channel.
    fn decode_rfss_sts_bcst(&self) -> TsbkMessage {
        let mut full = [0u8; 12];
        full[2..10].copy_from_slice(&self.payload);
        let lra = self.bits(&full, 16, 8) as u8;
        let rfss_id = self.bits(&full, 40, 8) as u8;
        let site_id = self.bits(&full, 48, 8) as u8;
        // Channel = freq_band(4) | channel_number(12) -- packed into
        // a single 16-bit Channel(u16) where the top 4 bits are the
        // band (matches our Channel::identifier() / number() split).
        let channel = Channel(self.bits(&full, 56, 16) as u16);
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

    /// SECONDARY_CONTROL_CHANNEL_BROADCAST (0x39). SDRTrunk
    /// `SecondaryControlChannelBroadcast.java` absolute-bit-position
    /// layout (bit 0 = MSB of byte 0):
    ///
    /// | Field      | Bits  | Width |
    /// |------------|-------|-------|
    /// | RFSS       | 16-23 | 8     |
    /// | SITE       | 24-31 | 8     |
    /// | freq_band_a| 32-35 | 4     |
    /// | channel_a  | 36-47 | 12    |
    /// | service_a  | 48-55 | 8     |
    /// | freq_band_b| 56-59 | 4     |
    /// | channel_b  | 60-71 | 12    |
    /// | service_b  | 72-79 | 8     |
    ///
    /// Phase 6F.11.
    fn decode_secondary_cch_bcst(&self) -> TsbkMessage {
        let mut full = [0u8; 12];
        full[2..10].copy_from_slice(&self.payload);
        let rfss_id = self.bits(&full, 16, 8) as u8;
        let site_id = self.bits(&full, 24, 8) as u8;
        let channel_a = Channel(self.bits(&full, 32, 16) as u16);
        let channel_b = Channel(self.bits(&full, 56, 16) as u16);
        TsbkMessage::SecondaryControlChannelBroadcast {
            rfss_id,
            site_id,
            channel_a,
            channel_b,
        }
    }

    /// SNDCP_DATA_CHANNEL_ANNOUNCEMENT_EXPLICIT (0x16). SDRTrunk
    /// `SNDCPDataChannelAnnouncementExplicit.java` layout:
    ///
    /// | Field          | Bits  | Width |
    /// |----------------|-------|-------|
    /// | data svc opts  | 16-23 | 8     |
    /// | autonomous flg | 24    | 1     |
    /// | requested flg  | 25    | 1     |
    /// | DL freq band   | 32-35 | 4     |
    /// | DL channel num | 36-47 | 12    |
    /// | UL freq band   | 48-51 | 4     |
    /// | UL channel num | 52-63 | 12    |
    /// | data acc ctrl  | 64-79 | 16    |
    ///
    /// Phase 6F.11.
    fn decode_sndcp_dch_ann_ex(&self) -> TsbkMessage {
        let mut full = [0u8; 12];
        full[2..10].copy_from_slice(&self.payload);
        let autonomous_access = self.bits(&full, 24, 1) != 0;
        let requested_access = self.bits(&full, 25, 1) != 0;
        let downlink_channel = Channel(self.bits(&full, 32, 16) as u16);
        let uplink_channel = Channel(self.bits(&full, 48, 16) as u16);
        let data_access_control = self.bits(&full, 64, 16) as u16;
        TsbkMessage::SndcpDataChannelAnnouncementExplicit {
            autonomous_access,
            requested_access,
            downlink_channel,
            uplink_channel,
            data_access_control,
        }
    }

    /// TDMA_SYNC_BROADCAST (0x30). SDRTrunk
    /// `SynchronizationBroadcast.java` layout:
    ///
    /// | Field           | Bits   | Width |
    /// |-----------------|--------|-------|
    /// | reserved        | 16-28  | 13    |
    /// | unlocked flag   | 29     | 1     |
    /// | year            | 40-46  | 7     |
    /// | month           | 47-50  | 4     |
    /// | day             | 51-55  | 5     |
    /// | hours           | 56-60  | 5     |
    /// | minutes         | 61-66  | 6     |
    /// | micro_slots     | 67-79  | 13    |
    ///
    /// Phase 6F.11. We expose just enough fields to display "system
    /// clock" status; full ISO 8601 formatting is left to the dashboard.
    /// Year is the offset from 2000 per the SDRTrunk decoder.
    fn decode_tdma_sync_bcst(&self) -> TsbkMessage {
        let mut full = [0u8; 12];
        full[2..10].copy_from_slice(&self.payload);
        // SDRTrunk uses bit 29 as `SYSTEM_TIME_NOT_LOCKED`. We invert
        // for `time_locked` so the value is true when good.
        let time_locked = self.bits(&full, 29, 1) == 0;
        let year = (self.bits(&full, 40, 7) + 2000) as u16;
        let month = self.bits(&full, 47, 4) as u8;
        let day = self.bits(&full, 51, 5) as u8;
        let hours = self.bits(&full, 56, 5) as u8;
        let minutes = self.bits(&full, 61, 6) as u8;
        let micro_slots = self.bits(&full, 67, 13) as u16;
        TsbkMessage::TdmaSyncBroadcast {
            time_locked,
            year,
            month,
            day,
            hours,
            minutes,
            micro_slots,
        }
    }

    /// TELEPHONE_INTERCONNECT_VOICE_CHANNEL_GRANT_UPDATE (0x09).
    /// SDRTrunk `TelephoneInterconnectVoiceChannelGrantUpdate.java`:
    ///
    /// | Field          | Bits  | Width |
    /// |----------------|-------|-------|
    /// | service opts   | 16-23 | 8     |
    /// | freq_band      | 24-27 | 4     |
    /// | channel num    | 28-39 | 12    |
    /// | call timer     | 40-55 | 16    |
    /// | any address    | 56-79 | 24    |
    ///
    /// Phase 6F.11. Call timer is in 100 ms units per SDRTrunk's
    /// `getCallTimer()` (`* 100` ms → seconds via `/ 10`).
    fn decode_tele_int_v_ch_grant_update(&self) -> TsbkMessage {
        let mut full = [0u8; 12];
        full[2..10].copy_from_slice(&self.payload);
        let channel = Channel(self.bits(&full, 24, 16) as u16);
        let call_timer_raw = self.bits(&full, 40, 16) as u16;
        // SDRTrunk: timer * 100ms; we expose seconds rounded down.
        let call_timer_secs = call_timer_raw / 10;
        let unit_id = RadioId(self.bits(&full, 56, 24) as u32);
        TsbkMessage::TelephoneInterconnectVoiceChannelGrantUpdate {
            channel,
            call_timer_secs,
            unit_id,
        }
    }

    /// UNIT_TO_UNIT_ANSWER_REQUEST (0x05). SDRTrunk
    /// `UnitToUnitAnswerRequest.java`:
    ///
    /// | Field          | Bits  | Width |
    /// |----------------|-------|-------|
    /// | service opts   | 16-23 | 8     |
    /// | reserved       | 24-31 | 8     |
    /// | target address | 32-55 | 24    |
    /// | source address | 56-79 | 24    |
    ///
    /// Phase 6F.11.
    fn decode_uu_ans_req(&self) -> TsbkMessage {
        let mut full = [0u8; 12];
        full[2..10].copy_from_slice(&self.payload);
        let target = RadioId(self.bits(&full, 32, 24) as u32);
        let source = RadioId(self.bits(&full, 56, 24) as u32);
        TsbkMessage::UnitToUnitAnswerRequest { target, source }
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
    fn test_ccitt80_crc_known_tsbk() {
        // Phase 6F.2j: known-good TSBK1 SEC_CCH_BROADCST captured from
        // SDRTrunk's .bits file via the Python replay script. Decoded
        // bytes after deinterleave + Viterbi (clean, metric=0).
        // Expected residual = 0xFFFF (Xored convention) per
        // CRCP25.correctCCITT80.
        let bytes = [0x39u8, 0x00, 0x01, 0x01, 0x04, 0xfd, 0x04, 0x05, 0xe5, 0x04, 0x1e, 0x97];
        let calc = ccitt80_crc(&bytes);
        let msg = u16::from_be_bytes([bytes[10], bytes[11]]);
        let residual = calc ^ msg;
        assert!(
            residual == 0 || residual == 0xFFFF,
            "expected residual 0 or 0xFFFF, got 0x{:04X} (calc=0x{:04X} msg=0x{:04X})",
            residual,
            calc,
            msg,
        );
    }

    #[test]
    fn test_opcode_parsing() {
        assert_eq!(TsbkOpcode::from(0x00), TsbkOpcode::GroupVoiceChannelGrant);
        // Phase 6F.4: 0x33 = IDEN_UPDATE_TDMA, 0x34 = IDEN_UPDATE_VUHF,
        // 0x3D = IDEN_UPDATE (standard FDMA, the one Clay County actually
        // broadcasts). Until 6F.4 we mapped 0x34 to IdentifierUpdate
        // which never matched real on-air TSBKs.
        assert_eq!(TsbkOpcode::from(0x33), TsbkOpcode::IdentifierUpdateTdma);
        assert_eq!(TsbkOpcode::from(0x34), TsbkOpcode::IdentifierUpdateVuhf);
        assert_eq!(TsbkOpcode::from(0x3D), TsbkOpcode::IdentifierUpdate);
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
                service_options,
            } => {
                assert_eq!(channel.0, 0x0639);
                assert_eq!(channel.identifier(), 0);
                assert_eq!(channel.number(), 0x639); // 1593
                assert_eq!(talkgroup.0, 0x012C); // 300
                assert_eq!(source.0, 1);
                // Phase 7C: synthetic test data has options=0
                // (clear voice, no emergency, no encryption).
                assert_eq!(service_options, 0x00);
                assert!(!service_options::is_encrypted(service_options));
                assert!(!service_options::is_emergency(service_options));
            }
            _ => panic!("Expected GroupVoiceChannelGrant"),
        }
    }

    /// Phase 7C: synthetic GRP_V_CH_GRANT with the encryption bit
    /// set in the service options byte. Verifies that the decoder
    /// reads payload[0] correctly and that the helpers in
    /// `service_options` mod return true for the right bit.
    #[test]
    fn test_grp_v_ch_grant_decode_encrypted() {
        let mut data = [0u8; 12];
        data[0] = 0x80; // LB=1, P=0, opcode=0x00
        data[1] = 0x00; // standard manufacturer
        // payload: options=0x40 (ENCRYPTED bit set)
        data[2] = 0x40;
        data[3] = 0x06;
        data[4] = 0x39;
        data[5] = 0x01;
        data[6] = 0x2C;
        data[7] = 0x00;
        data[8] = 0x00;
        data[9] = 0x01;
        data[10] = 0x00;
        data[11] = 0x00;

        let block = TsbkBlock::parse(&data);
        let msg = block.decode().unwrap();
        match msg {
            TsbkMessage::GroupVoiceChannelGrant {
                service_options,
                ..
            } => {
                assert_eq!(service_options, 0x40);
                assert!(service_options::is_encrypted(service_options));
                assert!(!service_options::is_emergency(service_options));
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
