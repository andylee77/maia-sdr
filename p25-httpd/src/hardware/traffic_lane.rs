//! Change 066: the traffic chains ("lanes") a core offers.
//!
//! Core 0.3.0 adds a second traffic chain (`traffic2_*`, doc/changes/064)
//! so two voice calls can be followed at once. Chain 1 serves the left
//! speaker's groups and chain 2 the right's. Portable so the lane choice
//! is host-tested; the register access lives in `fpga::LaneRegs`.

use std::fmt;
use std::str::FromStr;

use crate::hardware::core_version::CoreVersion;

/// One traffic chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lane {
    /// `traffic_*` (every core).
    One,
    /// `traffic2_*` (core 0.3.0 and later).
    Two,
}

impl Lane {
    pub const ALL: [Lane; 2] = [Lane::One, Lane::Two];

    pub fn index(self) -> usize {
        match self {
            Lane::One => 0,
            Lane::Two => 1,
        }
    }

    pub fn from_index(i: usize) -> Option<Lane> {
        Lane::ALL.get(i).copied()
    }

    /// Chain number as the UI and API show it (1 or 2).
    pub fn number(self) -> u8 {
        self.index() as u8 + 1
    }

    /// Ring / log label, matching the gateware prefixes.
    pub fn label(self) -> &'static str {
        match self {
            Lane::One => "traffic",
            Lane::Two => "traffic2",
        }
    }
}

impl fmt::Display for Lane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// `--traffic-chains`: how many traffic chains the follower may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChainsArg {
    /// Every chain the core and device tree provide.
    #[default]
    Auto,
    /// Chain 1 only (the pre-066 behaviour on any core).
    One,
    /// Both chains; falls back to one when chain 2 is missing.
    Two,
}

impl FromStr for ChainsArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(ChainsArg::Auto),
            "1" | "one" => Ok(ChainsArg::One),
            "2" | "two" => Ok(ChainsArg::Two),
            other => Err(format!("traffic chains {other:?}: expected auto, 1 or 2")),
        }
    }
}

/// Number of lanes the app runs. Chain 2 needs a 0.3.0 core (older cores
/// stall the CPU on its addresses) and its DMA node in the device tree.
pub fn lanes_available(ver: CoreVersion, node_present: bool, arg: ChainsArg) -> usize {
    let hw = ver.has_traffic2_chain() && node_present;
    match arg {
        ChainsArg::One => 1,
        ChainsArg::Auto | ChainsArg::Two if hw => 2,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_labels_and_indices() {
        assert_eq!(Lane::One.index(), 0);
        assert_eq!(Lane::Two.index(), 1);
        assert_eq!(Lane::from_index(1), Some(Lane::Two));
        assert_eq!(Lane::from_index(2), None);
        assert_eq!(Lane::Two.number(), 2);
        assert_eq!(Lane::One.label(), "traffic");
        assert_eq!(Lane::Two.to_string(), "traffic2");
    }

    #[test]
    fn chains_arg_parses() {
        assert_eq!("auto".parse::<ChainsArg>(), Ok(ChainsArg::Auto));
        assert_eq!("1".parse::<ChainsArg>(), Ok(ChainsArg::One));
        assert_eq!(" Two ".parse::<ChainsArg>(), Ok(ChainsArg::Two));
        assert!("3".parse::<ChainsArg>().is_err());
    }

    #[test]
    fn lanes_need_core_node_and_arg() {
        let old = CoreVersion::new(0, 2, 0);
        let new = CoreVersion::new(0, 3, 0);
        for arg in [ChainsArg::Auto, ChainsArg::One, ChainsArg::Two] {
            assert_eq!(lanes_available(old, true, arg), 1, "{arg:?} on 0.2.0");
            assert_eq!(lanes_available(new, false, arg), 1, "{arg:?} without node");
        }
        assert_eq!(lanes_available(new, true, ChainsArg::One), 1);
        assert_eq!(lanes_available(new, true, ChainsArg::Two), 2);
        assert_eq!(lanes_available(new, true, ChainsArg::Auto), 2);
    }
}
