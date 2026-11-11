//! What the API handlers reach: handles to the services, not the services' internals.

use std::sync::Arc;
use std::time::Instant;

use crate::audio::live::Audio;
use crate::services::atsc::sweep::Atsc;
use crate::services::burst::BurstCheck;
use crate::services::clock::Clock;
use crate::services::crystal::Crystal;
use crate::services::discovery::sweep::Discovery;
use crate::services::files::Files;
use crate::services::history::History;
use crate::services::mode::Modes;
use crate::services::notices::Notices;
use crate::services::packet_data::PacketData;
use crate::services::recordings::Recordings;
use crate::services::system::SystemHealth;

use tokio::sync::Mutex;

use super::radio::{HardwareInfo, RadioTuner};
use crate::radio::hw::Hardware;
use crate::radio::lease::RadioLease;
use crate::services::config::{Config, Paths};
use crate::services::events::EventLog;
use crate::trunking::receivers::Receivers;
use crate::trunking::site::LiveSite;
use crate::trunking::trunk::Trunking;

pub struct AppState {
    pub paths: Paths,
    pub config: Arc<Mutex<Config>>,
    pub tuner: Arc<RadioTuner>,
    pub live: Arc<LiveSite<Hardware>>,
    pub lease: RadioLease,
    pub receivers: Arc<Receivers>,
    pub trunking: Arc<Trunking>,
    pub audio: Arc<Audio>,
    pub recordings: Arc<Recordings>,
    pub history: Arc<History>,
    /// The downloads of what is on the card.
    pub files: Arc<Files>,
    pub discovery: Arc<Discovery>,
    /// The unit's mode, and ATSC mode's channel finder.
    pub modes: Arc<Modes>,
    pub atsc: Arc<Atsc>,
    /// The radio core's burst timing check (a diagnostic).
    pub burst: Arc<BurstCheck>,
    pub notices: Notices,
    /// The live site's talkgroups of the last week, kept between reads of the history.
    pub heard: crate::api::v1::talkgroups::HeardWeek,
    pub clock: Arc<Clock>,
    pub crystal: Arc<Crystal<Hardware>>,
    pub packet_data: Arc<PacketData>,
    pub system: Arc<SystemHealth>,
    pub log: Arc<EventLog>,
    pub hardware: HardwareInfo,
    pub started: Instant,
    /// Shut down as on SIGTERM; the init script's loop starts the scanner again.
    pub restart: Arc<tokio::sync::Notify>,
}
