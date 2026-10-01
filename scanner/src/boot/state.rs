//! What the API handlers reach: handles to the services, not the services' internals.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use super::radio::{HardwareInfo, RadioTuner};
use crate::radio::hw::Hardware;
use crate::radio::lease::RadioLease;
use crate::services::config::{Config, Paths};
use crate::trunking::site::LiveSite;

pub struct AppState {
    pub paths: Paths,
    pub config: Arc<Mutex<Config>>,
    pub tuner: Arc<RadioTuner>,
    pub live: Arc<LiveSite<Hardware>>,
    pub lease: RadioLease,
    pub hardware: HardwareInfo,
    pub started: Instant,
}
