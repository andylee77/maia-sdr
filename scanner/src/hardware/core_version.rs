//! The P25 core's `version` register (major.minor.bugfix), read once when the core opens. It
//! says which gateware behaviours exist, so one binary runs on every bitstream in use.

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
    /// First core whose LSM chains hold PLL and timing while the AGC idle gate sees no signal.
    pub const SIGNAL_HOLD: CoreVersion = CoreVersion::new(0, 2, 0);
    /// First core with the second traffic chain (`traffic2_*` banks, interrupt bit 8).
    pub const TRAFFIC2: CoreVersion = CoreVersion::new(0, 3, 0);

    pub const fn new(major: u8, minor: u8, bugfix: u8) -> Self {
        CoreVersion { major, minor, bugfix }
    }

    pub fn has_lsm_signal_hold(self) -> bool {
        self >= Self::SIGNAL_HOLD
    }

    /// On older cores the `traffic2_*` addresses are vacant and reading them stalls the CPU,
    /// so every chain-2 access is gated on this.
    pub fn has_traffic2_chain(self) -> bool {
        self >= Self::TRAFFIC2
    }

    /// LSM PLL accumulator clamp in Q2.13.
    pub fn pll_clamp_q213(self) -> i32 {
        if self.has_lsm_signal_hold() { PLL_CLAMP_Q213_HOLD } else { PLL_CLAMP_Q213_LEGACY }
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
    fn features_start_at_their_versions() {
        assert!(!CoreVersion::new(0, 1, 255).has_lsm_signal_hold());
        assert!(CoreVersion::new(0, 2, 0).has_lsm_signal_hold());
        assert!(!CoreVersion::new(0, 2, 255).has_traffic2_chain());
        assert!(CoreVersion::new(0, 3, 0).has_traffic2_chain());
        assert!(CoreVersion::new(1, 0, 0).has_traffic2_chain());
    }

    #[test]
    fn clamp_follows_the_gateware() {
        assert_eq!(CoreVersion::new(0, 1, 0).pll_clamp_q213(), 8579);
        assert_eq!(CoreVersion::new(0, 2, 0).pll_clamp_q213(), 5325);
        let q13 = |rad: f64| (rad * 8192.0).round() as i32;
        assert_eq!(PLL_CLAMP_Q213_LEGACY, q13(std::f64::consts::PI / 3.0));
        assert_eq!(PLL_CLAMP_Q213_HOLD, q13(0.65));
        assert_eq!(CoreVersion::new(0, 2, 0).to_string(), "0.2.0");
    }
}
