//! TSBK opcode dispatch + per-opcode handlers.
//!
//! Every TSBK opcode landing in the decoder flows through
//! `handle_tsbk` here: grant store updates (`bands`, `grants`),
//! event broadcast (`event_tx`, `event_log`), and per-variant
//! rendering into `p25_json::TsbkEvent` via `tsbk_to_event_inner`.

use std::time::Instant;

use p25_json;

use super::*;
use crate::protocol::p25::tsbk::TsbkMessage;

impl ControlChannelDecoder {
    pub fn handle_tsbk(&mut self, block_idx: u8, msg: TsbkMessage) {
        match &msg {
            TsbkMessage::NetworkStatus {
                wacn,
                system_id,
                channel,
            } => {
                self.system.wacn = Some(*wacn);
                self.system.system_id = Some(*system_id);
                self.system.control_channel = Some(*channel);
            }
            TsbkMessage::RfssStatus {
                lra,
                rfss_id,
                site_id,
                channel,
            } => {
                self.system.lra = Some(*lra);
                self.system.rfss_id = Some(*rfss_id);
                self.system.site_id = Some(*site_id);
                self.system.control_channel = Some(*channel);
            }
            TsbkMessage::IdentifierUpdate { .. } => {
                if let Some(band) = FrequencyBand::from_tsbk(&msg) {
                    self.bands.insert(band.identifier, band);
                }
            }
            TsbkMessage::GroupVoiceChannelGrant {
                channel,
                talkgroup,
                source,
                service_options,
            } => {
                let freq = self.channel_to_frequency(*channel);
                // Drop any prior grant for this TG on a different channel.
                // TSBK carries fresh source + service_options so preserved
                // values are discarded -- new call's caller/encryption win.
                let _ = self.take_other_grants_for_talkgroup(*talkgroup);
                let encrypted =
                    crate::protocol::p25::tsbk::service_options::is_encrypted(*service_options);
                let emergency =
                    crate::protocol::p25::tsbk::service_options::is_emergency(*service_options);
                let grant = GrantInfo {
                    channel: *channel,
                    talkgroup: *talkgroup,
                    source: Some(*source),
                    frequency_hz: freq,
                    timestamp: Instant::now(),
                    encrypted,
                    emergency,
                };
                self.emit_grant_event(&grant);
                self.grants.insert(channel.0, grant);
            }
            // Backup CCH A/B channels for the trunking failover view.
            // RFSS/SITE overwrite (always identical to primary in practice).
            TsbkMessage::SecondaryControlChannelBroadcast {
                rfss_id,
                site_id,
                channel_a,
                channel_b,
            } => {
                self.system.rfss_id = Some(*rfss_id);
                self.system.site_id = Some(*site_id);
                self.system.secondary_cch_a = Some(*channel_a);
                self.system.secondary_cch_b = Some(*channel_b);
            }
            TsbkMessage::SndcpDataChannelAnnouncementExplicit {
                downlink_channel,
                uplink_channel,
                ..
            } => {
                self.system.sndcp_downlink_channel = Some(*downlink_channel);
                self.system.sndcp_uplink_channel = Some(*uplink_channel);
            }
            // Snapshot system clock for the activity feed / debug.
            TsbkMessage::TdmaSyncBroadcast {
                time_locked,
                year,
                month,
                day,
                hours,
                minutes,
                ..
            } => {
                self.system.last_sync_clock =
                    Some((*year, *month, *day, *hours, *minutes, *time_locked));
            }
            // Unit-to-phone grant (no talkgroup). Event-feed only;
            // `grants` is talkgroup-keyed.
            TsbkMessage::TelephoneInterconnectVoiceChannelGrantUpdate {
                ..
            } => {}
            // Private call paging. Pure event for the activity feed.
            TsbkMessage::UnitToUnitAnswerRequest { .. } => {}
            // GVCG_UPDT_EXPLICIT (0x03) carries its own service_options
            // byte, unlike plain GVCG_UPDT (0x02). SDRTrunk extracts the
            // encryption bit here; without this branch we'd drop the
            // grant and miss the encrypted flag on sites that use this
            // variant. transmit_channel is the grant channel (voice).
            TsbkMessage::GroupVoiceChannelGrantUpdateExplicit {
                transmit_channel,
                receive_channel: _,
                talkgroup,
                service_options,
            } => {
                let freq = self.channel_to_frequency(*transmit_channel);
                let _ = self.take_other_grants_for_talkgroup(*talkgroup);
                let encrypted =
                    crate::protocol::p25::tsbk::service_options::is_encrypted(*service_options);
                let emergency =
                    crate::protocol::p25::tsbk::service_options::is_emergency(*service_options);
                let grant = GrantInfo {
                    channel: *transmit_channel,
                    talkgroup: *talkgroup,
                    // GVCG_UPDT_EXP doesn't carry a source RadioId.
                    // Grant follower keys off TG + encryption, not source.
                    source: None,
                    frequency_hz: freq,
                    timestamp: Instant::now(),
                    encrypted,
                    emergency,
                };
                self.emit_grant_event(&grant);
                self.grants.insert(transmit_channel.0, grant);
            }
            TsbkMessage::GroupVoiceChannelGrantUpdate {
                channel_a,
                talkgroup_a,
                channel_b,
                talkgroup_b,
            } => {
                let freq_a = self.channel_to_frequency(*channel_a);
                // Drop prior grant for talkgroup_a on a different channel
                // and PRESERVE source + encryption + emergency: the update
                // TSBK doesn't carry service_options or a source, so without
                // this we'd wipe both the caller ID and the encryption flag
                // recorded from the original GroupVoiceChannelGrant.
                let preserved_a =
                    self.take_other_grants_for_talkgroup(*talkgroup_a);
                let grant_a = GrantInfo {
                    channel: *channel_a,
                    talkgroup: *talkgroup_a,
                    source: preserved_a.source,
                    frequency_hz: freq_a,
                    timestamp: Instant::now(),
                    encrypted: preserved_a.encrypted,
                    emergency: preserved_a.emergency,
                };
                self.emit_grant_event(&grant_a);
                self.grants.insert(channel_a.0, grant_a);
                if talkgroup_b.0 != 0 {
                    let freq_b = self.channel_to_frequency(*channel_b);
                    let preserved_b =
                        self.take_other_grants_for_talkgroup(*talkgroup_b);
                    let grant_b = GrantInfo {
                        channel: *channel_b,
                        talkgroup: *talkgroup_b,
                        source: preserved_b.source,
                        frequency_hz: freq_b,
                        timestamp: Instant::now(),
                        encrypted: preserved_b.encrypted,
                        emergency: preserved_b.emergency,
                    };
                    self.emit_grant_event(&grant_b);
                    self.grants.insert(channel_b.0, grant_b
                    );
                }
            }
            _ => {}
        }

        // Broadcast event over WebSocket + mirror into structured
        // event_log so /api/log exports match the dashboard feed.
        // Every FEC-passed TSBK -> one Grant-category entry, matching
        // SDRTrunk's `decoded_messages.log` (one line per decode).
        let tsbk_event = self.tsbk_to_event(block_idx, &msg);
        if let Some(ref tx) = self.event_tx {
            if let Ok(json) = serde_json::to_string(&tsbk_event) {
                let _ = tx.send(json);
            }
        }
        if let Some(ref log) = self.event_log {
            let summary = tsbk_event.summary.clone();
            let fields = serde_json::to_value(&tsbk_event)
                .unwrap_or(serde_json::Value::Null);
            log.push(
                crate::services::event_log::LogCategory::Grant,
                summary,
                fields,
            );
        }

        self.recent_messages.push((Instant::now(), block_idx, msg));
        if self.recent_messages.len() > self.max_recent {
            self.recent_messages.remove(0);
        }
    }

    /// Convert a TSBK message to a WebSocket event. Each event is
    /// prefixed with the originating block label ("TSBK1"/"TSBK2"/
    /// "TSBK3") to match SDRTrunk's `decoded_messages.log` format.
    fn tsbk_to_event(&self, block_idx: u8, msg: &TsbkMessage) -> p25_json::TsbkEvent {
        let mut event = self.tsbk_to_event_inner(block_idx, msg);
        // Rewrite `[TSBK2] TG:00300 ...` -> `TSBK2 GRP_VCH_GRANT TG:00300 ...`
        // to match SDRTrunk's `TSBK<N> <OPCODE_NAME> <payload>` format.
        let block_label = match block_idx {
            0 => "TSBK1",
            1 => "TSBK2",
            2 => "TSBK3",
            _ => "TSBK?",
        };
        let old = format!("[{}] ", block_label);
        let new = format!("{} {} ", block_label, event.event_type);
        if event.summary.starts_with(&old) {
            event.summary = event.summary.replacen(&old, &new, 1);
        } else {
            event.summary = format!(
                "{} {} {}", block_label, event.event_type, event.summary
            );
        }
        event
    }

    fn tsbk_to_event_inner(&self, block_idx: u8, msg: &TsbkMessage) -> p25_json::TsbkEvent {
        let now = chrono_timestamp();
        let block_label = match block_idx {
            0 => "TSBK1",
            1 => "TSBK2",
            2 => "TSBK3",
            _ => "TSBK?",
        };
        let block_prefix = format!("[{}] ", block_label);
        match msg {
            TsbkMessage::GroupVoiceChannelGrant {
                channel,
                talkgroup,
                source,
                service_options,
            } => {
                let freq = self.channel_to_frequency(*channel);
                let enc_marker = if crate::protocol::p25::tsbk::service_options::is_encrypted(*service_options) {
                    " [ENC]"
                } else {
                    ""
                };
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "GRP_VCH_GRANT".into(),
                    summary: format!(
                        "{}TG:{:05} SRC:{:05} -> {} ({:.4} MHz){}",
                        block_prefix,
                        talkgroup.0,
                        source.0,
                        channel,
                        freq.unwrap_or(0) as f64 / 1e6,
                        enc_marker
                    ),
                    talkgroup: Some(talkgroup.0),
                    talkgroup_alias: self.aliases.get(&talkgroup.0).cloned(),
                    channel: Some(format!("{}", channel)),
                    frequency_mhz: freq.map(|f| f as f64 / 1e6),
                    source: Some(source.0),
                }
            }
            TsbkMessage::GroupVoiceChannelGrantUpdate {
                channel_a,
                talkgroup_a,
                ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "GRP_VCH_GRNT_UPD".into(),
                summary: format!("{}TG:{:05} -> {}", block_prefix, talkgroup_a.0, channel_a),
                talkgroup: Some(talkgroup_a.0),
                talkgroup_alias: self.aliases.get(&talkgroup_a.0).cloned(),
                channel: Some(format!("{}", channel_a)),
                frequency_mhz: self
                    .channel_to_frequency(*channel_a)
                    .map(|f| f as f64 / 1e6),
                source: None,
            },
            TsbkMessage::GroupVoiceChannelGrantUpdateExplicit {
                transmit_channel,
                talkgroup,
                service_options,
                ..
            } => {
                let freq = self.channel_to_frequency(*transmit_channel);
                let enc_marker = if crate::protocol::p25::tsbk::service_options::is_encrypted(*service_options) {
                    " [ENC]"
                } else {
                    ""
                };
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "GRP_VCH_GRNT_UPD_EXP".into(),
                    summary: format!(
                        "{}TG:{:05} -> {} ({:.4} MHz){}",
                        block_prefix,
                        talkgroup.0,
                        transmit_channel,
                        freq.unwrap_or(0) as f64 / 1e6,
                        enc_marker
                    ),
                    talkgroup: Some(talkgroup.0),
                    talkgroup_alias: self.aliases.get(&talkgroup.0).cloned(),
                    channel: Some(format!("{}", transmit_channel)),
                    frequency_mhz: freq.map(|f| f as f64 / 1e6),
                    source: None,
                }
            }
            TsbkMessage::NetworkStatus {
                wacn,
                system_id,
                channel,
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "NET_STS_BCAST".into(),
                summary: format!("{}WACN:{:05X} SYS:{:03X} CH:{}", block_prefix, wacn, system_id, channel),
                talkgroup: None,
                talkgroup_alias: None,
                channel: Some(format!("{}", channel)),
                frequency_mhz: None,
                source: None,
            },
            TsbkMessage::RfssStatus {
                rfss_id, site_id, ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "RFSS_STS_BCAST".into(),
                summary: format!("{}RFSS:{:02} SITE:{:02}", block_prefix, rfss_id, site_id),
                talkgroup: None,
                talkgroup_alias: None,
                channel: None,
                frequency_mhz: None,
                source: None,
            },
            TsbkMessage::IdentifierUpdate {
                identifier,
                base_frequency,
                channel_spacing,
                ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "IDEN_UPDATE".into(),
                summary: format!(
                    "{}Band:{} base:{:.5} MHz spacing:{} Hz",
                    block_prefix,
                    identifier,
                    *base_frequency as f64 / 1e6,
                    channel_spacing
                ),
                talkgroup: None,
                talkgroup_alias: None,
                channel: None,
                frequency_mhz: Some(*base_frequency as f64 / 1e6),
                source: None,
            },
            TsbkMessage::AdjacentStatus {
                system_id,
                rfss_id,
                site_id,
                ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "ADJ_STS_BCAST".into(),
                summary: format!(
                    "{}SYS:{:03X} RFSS:{:02} SITE:{:02}",
                    block_prefix, system_id, rfss_id, site_id
                ),
                talkgroup: None,
                talkgroup_alias: None,
                channel: None,
                frequency_mhz: None,
                source: None,
            },
            TsbkMessage::SecondaryControlChannelBroadcast {
                channel_a, channel_b, ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "SCCB_EXP".into(),
                summary: format!(
                    "{}A:{} B:{}",
                    block_prefix, channel_a, channel_b
                ),
                talkgroup: None,
                talkgroup_alias: None,
                channel: Some(format!("{}", channel_a)),
                frequency_mhz: self
                    .channel_to_frequency(*channel_a)
                    .map(|f| f as f64 / 1e6),
                source: None,
            },
            TsbkMessage::SndcpDataChannelAnnouncementExplicit {
                downlink_channel,
                uplink_channel,
                ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "SNDCP_DCH_ANN_EX".into(),
                summary: format!(
                    "{}DL:{} UL:{}",
                    block_prefix, downlink_channel, uplink_channel
                ),
                talkgroup: None,
                talkgroup_alias: None,
                channel: Some(format!("{}", downlink_channel)),
                frequency_mhz: self
                    .channel_to_frequency(*downlink_channel)
                    .map(|f| f as f64 / 1e6),
                source: None,
            },
            TsbkMessage::TdmaSyncBroadcast {
                year, month, day, hours, minutes, time_locked, ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "TDMA_SYNC_BCST".into(),
                summary: format!(
                    "{}{:04}-{:02}-{:02} {:02}:{:02} {}",
                    block_prefix, year, month, day, hours, minutes,
                    if *time_locked { "LOCKED" } else { "UNLOCKED" }
                ),
                talkgroup: None,
                talkgroup_alias: None,
                channel: None,
                frequency_mhz: None,
                source: None,
            },
            TsbkMessage::TelephoneInterconnectVoiceChannelGrantUpdate {
                channel, call_timer_secs, unit_id,
            } => {
                let freq = self.channel_to_frequency(*channel);
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "TEL_INT_V_CH_GRANT_UPD".into(),
                    summary: format!(
                        "{}UNIT:{} CH:{} ({:.4} MHz) timer:{}s",
                        block_prefix, unit_id, channel,
                        freq.unwrap_or(0) as f64 / 1e6,
                        call_timer_secs,
                    ),
                    talkgroup: None,
                    talkgroup_alias: None,
                    channel: Some(format!("{}", channel)),
                    frequency_mhz: freq.map(|f| f as f64 / 1e6),
                    source: Some(unit_id.0),
                }
            }
            TsbkMessage::UnitToUnitAnswerRequest { target, source } => {
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "UU_ANS_REQ".into(),
                    summary: format!(
                        "{}TGT:{} SRC:{}",
                        block_prefix, target, source
                    ),
                    talkgroup: None,
                    talkgroup_alias: None,
                    channel: None,
                    frequency_mhz: None,
                    source: Some(source.0),
                }
            }
            // Registration / affiliation / SNDCP data / radio monitor /
            // FNE ack / vendor-specific: event feed only, not grant store.
            TsbkMessage::RadioUnitMonitorCommand { source, target } => {
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "RAD_MON_CMD".into(),
                    summary: format!(
                        "{}SRC:{} TGT:{}", block_prefix, source, target
                    ),
                    talkgroup: None,
                    talkgroup_alias: None,
                    channel: None,
                    frequency_mhz: None,
                    source: Some(source.0),
                }
            }
            TsbkMessage::SndcpDataChannelGrant {
                downlink_channel, uplink_channel, target, ..
            } => {
                let freq = self.channel_to_frequency(*downlink_channel);
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "SNDCP_DCH_GRANT".into(),
                    summary: format!(
                        "{}DL:{} UL:{} TGT:{}",
                        block_prefix, downlink_channel, uplink_channel, target,
                    ),
                    talkgroup: None,
                    talkgroup_alias: None,
                    channel: Some(format!("{}", downlink_channel)),
                    frequency_mhz: freq.map(|f| f as f64 / 1e6),
                    source: Some(target.0),
                }
            }
            TsbkMessage::SndcpDataPageRequest { target, source, .. } => {
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "SNDCP_DCH_PAG_RQ".into(),
                    summary: format!(
                        "{}TGT:{} SRC:{}", block_prefix, target, source
                    ),
                    talkgroup: None,
                    talkgroup_alias: None,
                    channel: None,
                    frequency_mhz: None,
                    source: Some(source.0),
                }
            }
            TsbkMessage::AcknowledgeResponseFne {
                service_type, source, target, ..
            } => {
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "ACK_RESP".into(),
                    summary: format!(
                        "{}SVC:0x{:02X} SRC:{} TGT:{}",
                        block_prefix, service_type, source, target
                    ),
                    talkgroup: None,
                    talkgroup_alias: None,
                    channel: None,
                    frequency_mhz: None,
                    source: Some(source.0),
                }
            }
            TsbkMessage::GroupAffiliationResponse {
                response, group, target, ..
            } => {
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "GRP_AFF_RSP".into(),
                    summary: format!(
                        "{}RSP:{} TG:{} TGT:{}",
                        block_prefix, response, group, target
                    ),
                    talkgroup: Some(group.0),
                    talkgroup_alias: self.aliases.get(&group.0).cloned(),
                    channel: None,
                    frequency_mhz: None,
                    source: Some(target.0),
                }
            }
            TsbkMessage::GroupAffiliationQuery { target, source } => {
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "GRP_AFF_Q".into(),
                    summary: format!(
                        "{}TGT:{} SRC:{}", block_prefix, target, source
                    ),
                    talkgroup: None,
                    talkgroup_alias: None,
                    channel: None,
                    frequency_mhz: None,
                    source: Some(source.0),
                }
            }
            TsbkMessage::LocationRegistrationResponse {
                response, group, rfss_id, site_id, target,
            } => {
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "LOC_RG_RSP".into(),
                    summary: format!(
                        "{}RSP:{} TG:{} RFSS:{:02} SITE:{:02} TGT:{}",
                        block_prefix, response, group, rfss_id, site_id, target
                    ),
                    talkgroup: Some(group.0),
                    talkgroup_alias: self.aliases.get(&group.0).cloned(),
                    channel: None,
                    frequency_mhz: None,
                    source: Some(target.0),
                }
            }
            TsbkMessage::UnitRegistrationResponse {
                response, system_id, source_id, source_address,
            } => {
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "U_REG_RSP".into(),
                    summary: format!(
                        "{}RSP:{} SYS:{:03X} SRC_ID:{} SRC_ADDR:{}",
                        block_prefix, response, system_id, source_id, source_address
                    ),
                    talkgroup: None,
                    talkgroup_alias: None,
                    channel: None,
                    frequency_mhz: None,
                    source: Some(source_address.0),
                }
            }
            TsbkMessage::UnitDeRegistrationAcknowledge {
                wacn, system_id, target,
            } => {
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "U_DE_REG_ACK".into(),
                    summary: format!(
                        "{}WACN:{:05X} SYS:{:03X} TGT:{}",
                        block_prefix, wacn, system_id, target
                    ),
                    talkgroup: None,
                    talkgroup_alias: None,
                    channel: None,
                    frequency_mhz: None,
                    source: Some(target.0),
                }
            }
            TsbkMessage::ManufacturerSpecific { mfid, opcode, .. } => {
                let vendor = match mfid {
                    0x90 => "MOT",
                    0xA4 => "HAR",
                    0x68 => "DVSI",
                    _ => "VEN",
                };
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "MFR_SPECIFIC".into(),
                    summary: format!(
                        "{}{} MFID:0x{:02X} OP:0x{:02X}",
                        block_prefix, vendor, mfid, opcode
                    ),
                    talkgroup: None,
                    talkgroup_alias: None,
                    channel: None,
                    frequency_mhz: None,
                    source: None,
                }
            }
        }
    }
}
