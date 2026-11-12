//! The aircraft module: ADS-B, the positions aircraft send of themselves at 1090 MHz. So far it
//! is the mode alone: it takes the radio from the scanner and gives it back, and nothing
//! listens yet. The receiver, the aircraft heard and their plot come next (the roadmap's
//! step 8).

use tokio::sync::Mutex;

use crate::services::mode::Hold;

#[derive(Default)]
pub struct Adsb {
    /// The mode's hold on the radio while it lasts.
    radio: Mutex<Option<Hold>>,
}

impl Adsb {
    /// The mode begins: the radio is this service's until `leave`.
    pub async fn enter(&self, hold: Hold) {
        *self.radio.lock().await = Some(hold);
    }

    /// The mode ends: its hold on the radio comes back, for the next mode or the scanner.
    pub async fn leave(&self) -> Option<Hold> {
        self.radio.lock().await.take()
    }
}
