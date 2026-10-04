//! Signal processing shared by the receivers: SDRTrunk's filter taps, the four-level FSK
//! building blocks, and the filters' multiply-accumulate runs.

pub mod fsk4;
pub mod run;
pub mod taps;

#[cfg(test)]
mod cost_tests;
