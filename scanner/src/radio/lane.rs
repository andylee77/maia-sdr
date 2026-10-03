//! The traffic lanes: the receivers that follow calls. Lane 1 and 2 are the radio core's lanes 1
//! and 2 (its lane 0 is the control channel's).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lane {
    One,
    Two,
}

impl Lane {
    pub const ALL: [Lane; 2] = [Lane::One, Lane::Two];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn number(self) -> u8 {
        self as u8 + 1
    }

    pub fn name(self) -> &'static str {
        match self {
            Lane::One => "lane 1",
            Lane::Two => "lane 2",
        }
    }

    /// The radio core's lane.
    pub fn core_lane(self) -> usize {
        self.number() as usize
    }
}

impl fmt::Display for Lane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lane {}", self.number())
    }
}
