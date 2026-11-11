//! SDRTrunk's playlist (`playlist/*.xml`): read, and written from the configuration, so the
//! scanner and SDRTrunk can share systems and names.
//!
//! A playlist has channels and aliases. A channel is one frequency to decode, or several that
//! SDRTrunk rotates through until one decodes, under a system name and a site name, with the name
//! of the alias list it uses. An alias belongs to a list and holds talkgroups, radios, a priority
//! and a Record mark as IDs. Both ways of spelling an element's kind are read: `type`, as
//! SDRTrunk writes it now, and `xsi:type`, as older files have it.
//!
//! `dump` (in the tests' build) is a playlist as lines of text, the same lines
//! `tools/sdrtrunk_playlist_reference.py` prints from SDRTrunk's own reader, so the two can be
//! compared.

pub mod import;
mod xml;

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::services::config::aliases::{Alias, AliasId, Side, LOWEST_PRIORITY};
use crate::services::config::systems::{Modulation, Protocol, System};

/// The playlist version SDRTrunk writes.
const VERSION: u32 = 4;
/// SDRTrunk's priority for an alias never followed.
const DO_NOT_MONITOR: i32 = -1;
const P25_DECODER: &str = "decodeConfigP25Phase1";
const DMR_DECODER: &str = "decodeConfigDMR";

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Playlist {
    pub channels: Vec<Channel>,
    pub aliases: Vec<ListAlias>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub system: String,
    pub site: String,
    pub name: String,
    /// SDRTrunk starts it by itself.
    pub enabled: bool,
    /// SDRTrunk's name for its decoder (`decodeConfigP25Phase1`).
    pub decoder: String,
    /// P25: `CQPSK` (simulcast) or `C4FM`.
    pub modulation: Option<String>,
    /// P25: calls of talkgroups with no priority are not followed.
    pub ignore_unmonitored: Option<bool>,
    /// The one frequency, or those rotated through.
    pub frequencies_hz: Vec<u64>,
    /// DMR: logical channel numbers to downlink frequencies.
    pub lcn_hz: BTreeMap<u16, u64>,
    pub alias_list: Option<String>,
}

impl Channel {
    /// The protocol the scanner follows it with, if it has one for the decoder.
    pub fn protocol(&self) -> Option<Protocol> {
        match self.decoder.as_str() {
            P25_DECODER => Some(Protocol::P25),
            DMR_DECODER => Some(Protocol::DmrTier3),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListAlias {
    /// The alias list it is in; empty when in none.
    pub list: String,
    pub name: String,
    pub group: Option<String>,
    /// As SDRTrunk keeps it: an ARGB integer, 0 when none was chosen.
    pub color: i32,
    pub icon: Option<String>,
    pub ids: Vec<Id>,
}

/// One of an alias's IDs. `protocol` is SDRTrunk's name for it (`APCO25`, `DMR`).
#[derive(Debug, Clone, PartialEq)]
pub enum Id {
    Talkgroup { protocol: String, value: u32 },
    TalkgroupRange { protocol: String, min: u32, max: u32 },
    Radio { protocol: String, value: u32 },
    RadioRange { protocol: String, min: u32, max: u32 },
    Priority(i32),
    Record,
    /// A kind the scanner has no use for (a stream, a tone, a status), by SDRTrunk's name.
    Other(String),
}

/// SDRTrunk's name for a protocol's IDs.
fn protocol_name(p: Protocol) -> &'static str {
    match p {
        Protocol::P25 => "APCO25",
        Protocol::DmrTier3 => "DMR",
    }
}

/// Read a playlist file.
pub fn parse(text: &str) -> Result<Playlist, String> {
    let root = xml::parse(text)?;
    if root.name != "playlist" {
        return Err(format!("not an SDRTrunk playlist: its element is <{}>, not <playlist>", root.name));
    }
    let mut out = Playlist::default();
    for el in &root.children {
        match el.name.as_str() {
            "channel" => out.channels.push(channel(el)),
            "alias" => out.aliases.push(alias(el)),
            _ => {}
        }
    }
    Ok(out)
}

fn channel(el: &xml::Element) -> Channel {
    let text = |name: &str| el.attr(name).unwrap_or("").to_string();
    let decode = el.child("decode_configuration");
    let source = el.child("source_configuration");
    let frequencies_hz = match source.and_then(|s| s.attr("type")) {
        Some("sourceConfigTuner") => source.and_then(|s| s.attr("frequency")).and_then(|f| f.parse().ok()).into_iter().collect(),
        Some("sourceConfigTunerMultipleFrequency") => source.into_iter().flat_map(|s| s.children("frequency")).filter_map(|f| f.text.parse().ok()).collect(),
        _ => Vec::new(),
    };
    let lcn_hz = decode
        .into_iter()
        .flat_map(|d| d.children("timeslot"))
        .filter_map(|t| Some((t.attr("lsn")?.parse().ok()?, t.attr("downlink")?.parse().ok()?)))
        .collect();
    Channel {
        system: text("system"),
        site: text("site"),
        name: text("name"),
        enabled: el.attr("enabled") == Some("true"),
        decoder: decode.and_then(|d| d.attr("type")).unwrap_or("").to_string(),
        modulation: decode.and_then(|d| d.attr("modulation")).map(str::to_string),
        ignore_unmonitored: decode.and_then(|d| d.attr("ignore_unmonitored_calls")).map(|v| v == "true"),
        frequencies_hz,
        lcn_hz,
        alias_list: el.child("alias_list_name").map(|l| l.text.clone()).filter(|l| !l.is_empty()),
    }
}

fn alias(el: &xml::Element) -> ListAlias {
    let text = |name: &str| el.attr(name).map(str::to_string);
    ListAlias {
        list: text("list").unwrap_or_default(),
        name: text("name").unwrap_or_default(),
        group: text("group"),
        color: el.attr("color").and_then(|c| c.parse().ok()).unwrap_or(0),
        icon: text("iconName"),
        ids: el.children("id").map(id).collect(),
    }
}

fn id(el: &xml::Element) -> Id {
    let kind = el.attr("type").unwrap_or("");
    let number = |name: &str| el.attr(name).and_then(|v| v.parse::<u32>().ok());
    let protocol = || el.attr("protocol").unwrap_or("").to_string();
    let other = || Id::Other(kind.to_string());
    match kind {
        "talkgroup" => number("value").map_or_else(other, |value| Id::Talkgroup { protocol: protocol(), value }),
        "radio" => number("value").map_or_else(other, |value| Id::Radio { protocol: protocol(), value }),
        "talkgroupRange" => number("min").zip(number("max")).map_or_else(other, |(min, max)| Id::TalkgroupRange { protocol: protocol(), min, max }),
        "radioRange" => number("min").zip(number("max")).map_or_else(other, |(min, max)| Id::RadioRange { protocol: protocol(), min, max }),
        "priority" => el.attr("priority").and_then(|p| p.parse().ok()).map_or_else(other, Id::Priority),
        "record" => Id::Record,
        _ => other(),
    }
}

#[cfg(test)]
impl Playlist {
    /// The playlist as lines: a line a channel, then a line an alias, each in the file's order.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        let list = |v: &[String]| v.join(",");
        for c in &self.channels {
            let freqs: Vec<String> = c.frequencies_hz.iter().map(u64::to_string).collect();
            let lcns: Vec<String> = c.lcn_hz.iter().map(|(n, f)| format!("{n}:{f}")).collect();
            let _ = writeln!(
                out,
                "channel|{}|{}|{}|enabled={}|{}|mod={}|ium={}|freqs={}|lcn={}|list={}",
                c.system,
                c.site,
                c.name,
                c.enabled,
                c.decoder,
                c.modulation.as_deref().unwrap_or("-"),
                c.ignore_unmonitored.map_or("-".to_string(), |v| v.to_string()),
                list(&freqs),
                list(&lcns),
                c.alias_list.as_deref().unwrap_or("")
            );
        }
        for a in &self.aliases {
            let ids: Vec<String> = a
                .ids
                .iter()
                .map(|id| match id {
                    Id::Talkgroup { protocol, value } => format!("talkgroup:{protocol}:{value}"),
                    Id::TalkgroupRange { protocol, min, max } => format!("talkgroupRange:{protocol}:{min}-{max}"),
                    Id::Radio { protocol, value } => format!("radio:{protocol}:{value}"),
                    Id::RadioRange { protocol, min, max } => format!("radioRange:{protocol}:{min}-{max}"),
                    Id::Priority(p) => format!("priority:{p}"),
                    Id::Record => "record".to_string(),
                    Id::Other(kind) => kind.clone(),
                })
                .collect();
            let _ = writeln!(
                out,
                "alias|{}|{}|{}|color={}|icon={}|{}",
                a.list,
                a.name,
                a.group.as_deref().unwrap_or(""),
                a.color,
                a.icon.as_deref().unwrap_or(""),
                ids.join(" ")
            );
        }
        out
    }
}

impl ListAlias {
    /// The alias as the scanner keeps one, for a system of `protocol`: its talkgroups and radios
    /// of that protocol, its priority (SDRTrunk's -1 is "never followed") and its Record mark.
    /// None when it has no name, or names no talkgroup or radio of the protocol.
    pub fn to_alias(&self, protocol: Protocol) -> Option<Alias> {
        let of = protocol_name(protocol);
        let mut ids = Vec::new();
        let (mut priority, mut do_not_monitor, mut record) = (None, false, false);
        for id in &self.ids {
            let alias_id = match id {
                Id::Talkgroup { protocol, value } if protocol == of => AliasId::Talkgroup { value: *value },
                Id::Radio { protocol, value } if protocol == of => AliasId::Radio { value: *value },
                Id::TalkgroupRange { protocol, min, max } if protocol == of && min <= max => AliasId::TalkgroupRange { min: *min, max: *max },
                Id::RadioRange { protocol, min, max } if protocol == of && min <= max => AliasId::RadioRange { min: *min, max: *max },
                Id::Priority(DO_NOT_MONITOR) => {
                    do_not_monitor = true;
                    continue;
                }
                Id::Priority(p) => {
                    priority = u8::try_from(*p).ok().filter(|p| (1..=LOWEST_PRIORITY).contains(p)).or(priority);
                    continue;
                }
                Id::Record => {
                    record = true;
                    continue;
                }
                _ => continue,
            };
            if !ids.contains(&alias_id) {
                ids.push(alias_id);
            }
        }
        let name = self.name.trim();
        if name.is_empty() || ids.is_empty() {
            return None;
        }
        let text = |v: &Option<String>| v.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
        Some(Alias {
            name: name.to_string(),
            group: text(&self.group),
            color: (self.color != 0).then(|| format!("#{:06x}", self.color as u32 & 0xFF_FFFF)),
            ids,
            priority,
            do_not_monitor,
            record,
            speaker: Side::Both,
            icon: text(&self.icon),
        })
    }
}

/// An alias's colour as SDRTrunk keeps one: opaque, or 0 for none.
fn color_of(alias: &Alias) -> i32 {
    alias.color.as_deref().and_then(|c| u32::from_str_radix(c.trim_start_matches('#'), 16).ok()).map_or(0, |rgb| (0xFF00_0000 | rgb) as i32)
}

/// The configuration as a playlist: a channel for each site, on its control channel and the
/// channels that take over from it, and each system's aliases as a list named after it.
///
/// What a playlist has no place for is left out: a talkgroup's tuner, a site's identity and the
/// channels it has learned. An alias both switched off and given a priority is written as
/// switched off, since SDRTrunk keeps one of the two.
pub fn export(systems: &[System]) -> String {
    let e = xml::escape;
    let mut out = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<playlist version=\"{VERSION}\">\n");
    let lists = list_names(systems);
    for (sys, list) in systems.iter().zip(&lists) {
        let protocol = protocol_name(sys.protocol);
        for a in &sys.aliases {
            let _ = write!(out, "  <alias name=\"{}\" color=\"{}\"", e(&a.name), color_of(a));
            if let Some(group) = &a.group {
                let _ = write!(out, " group=\"{}\"", e(group));
            }
            let _ = write!(out, " list=\"{}\"", e(list));
            if let Some(icon) = &a.icon {
                let _ = write!(out, " iconName=\"{}\"", e(icon));
            }
            out.push_str(">\n");
            for id in &a.ids {
                let _ = match *id {
                    AliasId::Talkgroup { value } => writeln!(out, "    <id type=\"talkgroup\" value=\"{value}\" protocol=\"{protocol}\"/>"),
                    AliasId::Radio { value } => writeln!(out, "    <id type=\"radio\" value=\"{value}\" protocol=\"{protocol}\"/>"),
                    AliasId::TalkgroupRange { min, max } => writeln!(out, "    <id type=\"talkgroupRange\" protocol=\"{protocol}\" min=\"{min}\" max=\"{max}\"/>"),
                    AliasId::RadioRange { min, max } => writeln!(out, "    <id type=\"radioRange\" protocol=\"{protocol}\" min=\"{min}\" max=\"{max}\"/>"),
                };
            }
            if let Some(p) = if a.do_not_monitor { Some(DO_NOT_MONITOR) } else { a.priority.map(i32::from) } {
                let _ = writeln!(out, "    <id type=\"priority\" priority=\"{p}\"/>");
            }
            if a.record {
                out.push_str("    <id type=\"record\"/>\n");
            }
            out.push_str("  </alias>\n");
        }
    }
    for (sys, list) in systems.iter().zip(&lists) {
        for site in &sys.sites {
            let _ = writeln!(out, "  <channel name=\"Control\" site=\"{}\" system=\"{}\" order=\"0\" enabled=\"false\">", e(&site.label), e(&sys.label));
            if site.control.alternates_hz.is_empty() {
                let _ = writeln!(out, "    <source_configuration type=\"sourceConfigTuner\" frequency=\"{}\" source_type=\"TUNER\"/>", site.control.freq_hz);
            } else {
                out.push_str("    <source_configuration type=\"sourceConfigTunerMultipleFrequency\" source_type=\"TUNER_MULTIPLE_FREQUENCIES\">\n");
                for f in std::iter::once(&site.control.freq_hz).chain(&site.control.alternates_hz) {
                    let _ = writeln!(out, "      <frequency>{f}</frequency>");
                }
                out.push_str("    </source_configuration>\n");
            }
            let _ = writeln!(out, "    <alias_list_name>{}</alias_list_name>", e(list));
            out.push_str("    <event_log_configuration/>\n    <aux_decode_configuration/>\n");
            match sys.protocol {
                Protocol::P25 => {
                    let modulation = match site.modulation {
                        Modulation::Lsm => " modulation=\"CQPSK\"",
                        Modulation::C4fm => " modulation=\"C4FM\"",
                        // Not probed yet: SDRTrunk's own choice.
                        Modulation::Auto => "",
                    };
                    let _ = writeln!(
                        out,
                        "    <decode_configuration type=\"{P25_DECODER}\"{modulation} ignore_unmonitored_calls=\"{}\"/>",
                        !sys.listening.follow_unmonitored
                    );
                }
                Protocol::DmrTier3 => {
                    let plan: Vec<(&u16, &u64)> = site.channel_plan.iter().flat_map(|p| &p.lcn_hz).collect();
                    if plan.is_empty() {
                        let _ = writeln!(out, "    <decode_configuration type=\"{DMR_DECODER}\"/>");
                    } else {
                        let _ = writeln!(out, "    <decode_configuration type=\"{DMR_DECODER}\">");
                        for (lcn, f) in plan {
                            let _ = writeln!(out, "      <timeslot lsn=\"{lcn}\" downlink=\"{f}\" uplink=\"0\"/>");
                        }
                        out.push_str("    </decode_configuration>\n");
                    }
                }
            }
            out.push_str("    <record_configuration/>\n  </channel>\n");
        }
    }
    out.push_str("</playlist>\n");
    out
}

/// Each system's alias list name: its own name, with its id where two systems share one.
fn list_names(systems: &[System]) -> Vec<String> {
    systems
        .iter()
        .map(|sys| if systems.iter().filter(|x| x.label == sys.label).count() > 1 { format!("{} ({})", sys.label, sys.id) } else { sys.label.clone() })
        .collect()
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
