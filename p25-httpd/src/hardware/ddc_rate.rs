//! Post-DDC sample rates, the single source for every rate label.
//!
//! The 2026-05-03 retune moved every `ddc_presets` preset from 62.5 kSPS
//! to 50 kSPS so the HDL `LsmDecimator2` /2 lands the LSM front end at
//! 25 kSPS, SDRTrunk's rate. Bench-verified 2026-09-27: the control
//! chain's post-DDC IQ carries the 4800 sym/s line at 0.0960 cycles per
//! sample, i.e. exactly 50 000 samples/s.

/// Complex samples/s at the DDC output of both chains (every preset).
pub const DDC_OUTPUT_RATE_HZ: u32 = 50_000;

/// LSM front end after the HDL's /2 decimator.
pub const LSM_INPUT_RATE_HZ: u32 = DDC_OUTPUT_RATE_HZ / 2;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::ddc_presets::PRESETS;

    #[test]
    fn every_preset_decimates_to_the_ddc_output_rate() {
        for p in PRESETS {
            assert_eq!(p.sample_rate_hz as usize % p.total_decim(), 0, "{}", p.name);
            assert_eq!(p.sample_rate_hz as usize / p.total_decim(),
                       DDC_OUTPUT_RATE_HZ as usize, "{}", p.name);
        }
    }

    #[test]
    fn lsm_front_end_matches_sdrtrunk() {
        assert_eq!(LSM_INPUT_RATE_HZ, 25_000);
    }
}
