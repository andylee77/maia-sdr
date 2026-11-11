//! The switch for libiio on the unit: its daemon (`iiod`, port 30431 on the network) and the USB
//! gadget's IIO function, which let another program use the board as a tuner, and so retune it
//! under the scanner. The firmware owns the switch (`iio-switch`: the choice kept on the flash,
//! the gadget changed to match, off unless switched on); this asks it and tells it. A system
//! without that command (a PC, an image from before it) has no switch to show.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde::Serialize;

/// What the firmware's switch says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct View {
    /// The system has the switch. False: nothing here switches libiio's daemon, and where the
    /// firmware has the daemon it runs always.
    pub available: bool,
    /// The choice kept: on at every start.
    pub wanted: bool,
    /// It runs now. Without `wanted`: switched on until the unit restarts (a bench does that).
    pub running: bool,
}

const NO_SWITCH: View = View { available: false, wanted: false, running: false };

pub struct Iiod {
    program: PathBuf,
    /// One change at a time: each takes the USB gadget off the bus and back.
    changing: Mutex<()>,
}

/// `wanted=on running=off`, as `iio-switch status` prints it.
fn parse(status: &str) -> Option<View> {
    let (mut wanted, mut running) = (None, None);
    for word in status.split_whitespace() {
        match word.split_once('=') {
            Some(("wanted", v)) => wanted = Some(v == "on"),
            Some(("running", v)) => running = Some(v == "on"),
            _ => {}
        }
    }
    Some(View { available: true, wanted: wanted?, running: running? })
}

impl Iiod {
    /// `program` is the firmware's command.
    pub fn new(program: &Path) -> Iiod {
        Iiod { program: program.to_path_buf(), changing: Mutex::new(()) }
    }

    /// What the switch says now. Blocking: it runs the firmware's command.
    pub fn view(&self) -> View {
        match Command::new(&self.program).arg("status").output() {
            Ok(out) if out.status.success() => parse(&String::from_utf8_lossy(&out.stdout)).unwrap_or(NO_SWITCH),
            _ => NO_SWITCH,
        }
    }

    /// Switch it. `kept` keeps the choice for every start; switched on without it, the daemon
    /// runs until the unit restarts and the choice kept stays as it was. Off is always kept.
    /// Blocking, for the second the gadget takes.
    pub fn set(&self, on: bool, kept: bool) -> Result<View, String> {
        let _one = self.changing.lock().unwrap_or_else(|e| e.into_inner());
        let word = match (on, kept) {
            (true, true) => "on",
            (true, false) => "once",
            (false, _) => "off",
        };
        let out = Command::new(&self.program).arg(word).output().map_err(|e| format!("{}: {e}", self.program.display()))?;
        let now = self.view();
        if !out.status.success() || !now.available || now.running != on {
            return Err(format!("libiio's daemon (iiod) was not switched {}: it is {}", if on { "on" } else { "off" }, if now.running { "running" } else { "not running" }));
        }
        Ok(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_read_from_what_the_firmware_prints() {
        assert_eq!(parse("wanted=on running=on\n"), Some(View { available: true, wanted: true, running: true }));
        assert_eq!(parse("wanted=off running=on"), Some(View { available: true, wanted: false, running: true }), "switched on until the restart");
        assert_eq!(parse("wanted=off running=off"), Some(View { available: true, wanted: false, running: false }));
        // Anything else is not the switch.
        assert_eq!(parse(""), None);
        assert_eq!(parse("wanted=on"), None);
        assert_eq!(parse("Usage: iio-switch {wanted|status|on|off|once}"), None);
    }

    #[test]
    fn a_system_without_the_command_has_no_switch() {
        let none = Iiod::new(Path::new("/nowhere/iio-switch"));
        assert_eq!(none.view(), NO_SWITCH);
        assert!(none.set(true, true).is_err());
    }
}
