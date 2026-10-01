//! What the API handlers reach: handles to the services, not the services' internals.

use std::sync::Arc;
use std::time::Instant;

use crate::audio::live::Audio;

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
    pub log: Arc<EventLog>,
    pub hardware: HardwareInfo,
    pub started: Instant,
}
