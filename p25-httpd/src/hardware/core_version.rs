//! P25 core `version` register (major.minor.bugfix, `p25_top._version`).
//!
//! Change 059: the PS reads it once at `IpCore::take` to learn which
//! gateware behaviours exist, so one p25-httpd binary runs correctly on
//! either bitstream (the Tezuka image and a bench BOOT.bin can differ).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CoreVersion {
    pub major: u8,
    pub minor: u8,
    pub bugfix: u8,
}

/// Traffic LSM PLL clamp (Q2.13) before 0.2.0: π/3 rad/symbol.
pub const PLL_CLAMP_Q213_LEGACY: i32 = 8579;
/// Traffic LSM PLL clamp (Q2.13) from 0.2.0: 0.65 rad/symbol.
pub const PLL_CLAMP_Q213_HOLD: i32 = 5325;

impl CoreVersion {
    /// First core with the LSM PLL/timing no-signal hold on the AGC idle
    /// gate and the 0.65 rad PLL clamp (doc/changes/059).
    pub const SIGNAL_HOLD: CoreVersion = CoreVersion { major: 0, minor: 2, bugfix: 0 };

    pub const fn new(major: u8, minor: u8, bugfix: u8) -> Self {
        CoreVersion { major, minor, bugfix }
    }

    /// The LSM chains hold their PLL and Gardner timing while the AGC
    /// idle gate reports no signal (no carrier-gap PLL trap).
    pub fn has_lsm_signal_hold(self) -> bool {
        self >= Self::SIGNAL_HOLD
    }

    /// LSM PLL accumulator clamp in Q2.13 (`lsm_pll_update.MAX_PLL_ABS`).
    pub fn pll_clamp_q213(self) -> i32 {
        if self.has_lsm_signal_hold() {
            PLL_CLAMP_Q213_HOLD
        } else {
            PLL_CLAMP_Q213_LEGACY
        }
    }
}

impl fmt::Display for CoreVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.bugfix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_hold_starts_at_0_2_0() {
        assert!(!CoreVersion::new(0, 1, 0).has_lsm_signal_hold());
        assert!(!CoreVersion::new(0, 1, 255).has_lsm_signal_hold());
        assert!(CoreVersion::new(0, 2, 0).has_lsm_signal_hold());
        assert!(CoreVersion::new(1, 0, 0).has_lsm_signal_hold());
    }

    #[test]
    fn clamp_follows_the_gateware() {
        assert_eq!(CoreVersion::new(0, 1, 0).pll_clamp_q213(), 8579);
        assert_eq!(CoreVersion::new(0, 2, 0).pll_clamp_q213(), 5325);
        // Both match their HDL constants (π/3 and 0.65 rad in Q2.13).
        let q13 = |rad: f64| (rad * 8192.0).round() as i32;
        assert_eq!(PLL_CLAMP_Q213_LEGACY, q13(std::f64::consts::PI / 3.0));
        assert_eq!(PLL_CLAMP_Q213_HOLD, q13(0.65));
        assert_eq!(CoreVersion::new(0, 2, 0).to_string(), "0.2.0");
    }
}
