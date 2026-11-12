//! Data mode: the unit watching the open-protocol data bands (315, 345, 433.92 and 902 to
//! 928 MHz) for the devices that transmit nearby (`doc/DATA_MODE.md`). So far it is the mode
//! alone: it takes the radio from the scanner and gives it back, and nothing listens yet. The
//! scan, the devices and the captures come with the study's later steps.

use tokio::sync::Mutex;

use crate::services::mode::Hold;

#[derive(Default)]
pub struct Data {
    /// The mode's hold on the radio while it lasts.
    radio: Mutex<Option<Hold>>,
}

impl Data {
    /// Data mode begins: the radio is this service's until `leave`.
    pub async fn enter(&self, hold: Hold) {
        *self.radio.lock().await = Some(hold);
    }

    /// Data mode ends: its hold on the radio comes back, for the next mode or the scanner.
    pub async fn leave(&self) -> Option<Hold> {
        self.radio.lock().await.take()
    }
}
