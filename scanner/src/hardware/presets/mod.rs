//! DDC presets: the AD9361 sample rate and the matching three-stage FIR decimator, plus the
//! coefficient RAM image each stage needs. Every preset decimates to `DDC_OUTPUT_RATE_HZ`.

mod table;

pub use table::{find_preset, DdcPreset, DEFAULT_PRESET, PRESETS};

use anyhow::{bail, Result};

/// Complex samples/s at the output of every DDC, for every preset.
pub const DDC_OUTPUT_RATE_HZ: u32 = 50_000;

/// The LSM front end, after the gateware's /2 decimator: SDRTrunk's rate.
pub const LSM_INPUT_RATE_HZ: u32 = DDC_OUTPUT_RATE_HZ / 2;

/// The three FIR stages share one coefficient RAM: FIR1 (FIR4DSP, folded) at 0-255, FIR2
/// (FIR2DSP) at 256-383 and FIR3 (FIR4DSP) at 512-767.
pub const FIR4_RAM: usize = 256;
pub const FIR2_RAM: usize = 128;
pub const FIR_BASE: [usize; 3] = [0, 256, 512];

/// A stage's fields in the `*_ddc_decimation` / `*_ddc_control` registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageFields {
    pub decimation: u8,
    pub operations_minus_one: u8,
    /// FIR4DSP stages only (FIR2DSP has no fold).
    pub odd_operations: bool,
}

/// What a preset loads into a DDC: three RAM images (at `FIR_BASE`) and the stage fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirLoad {
    pub ram: [Vec<i32>; 3],
    pub stages: [StageFields; 3],
}

impl FirLoad {
    pub fn of(p: &DdcPreset) -> Result<FirLoad> {
        Ok(FirLoad {
            ram: [
                fir4dsp_ram(p.fir1_coeffs, p.decim1)?,
                fir2dsp_ram(p.fir2_coeffs, p.decim2)?,
                fir4dsp_ram(p.fir3_coeffs, p.decim3)?,
            ],
            stages: [
                fir4dsp_fields(p.fir1_coeffs, p.decim1)?,
                fir2dsp_fields(p.fir2_coeffs, p.decim2)?,
                fir4dsp_fields(p.fir3_coeffs, p.decim3)?,
            ],
        })
    }
}

/// RAM image of a folded FIR4DSP stage (FIR1, FIR3): polyphase branches, two taps per DSP
/// operation, the odd taps in the upper half.
pub fn fir4dsp_ram(coefficients: &[i32], decimation: usize) -> Result<Vec<i32>> {
    let branch_len = coefficients.len().div_ceil(decimation);
    let operations = branch_len.div_ceil(2);
    if operations * decimation > FIR4_RAM / 2 {
        bail!("FIR4DSP coefficients too long for RAM");
    }
    Ok((0..FIR4_RAM)
        .map(|addr| {
            let (off, fold) = if addr >= FIR4_RAM / 2 { (1, FIR4_RAM / 2) } else { (0, 0) };
            let k = (addr - fold) / operations;
            if k >= decimation {
                return 0;
            }
            let j = (addr - fold) % operations;
            let n = (2 * j + off) * decimation + (decimation - 1 - k);
            coefficients.get(n).copied().unwrap_or(0)
        })
        .collect())
}

/// RAM image of an unfolded FIR2DSP stage (FIR2).
pub fn fir2dsp_ram(coefficients: &[i32], decimation: usize) -> Result<Vec<i32>> {
    let operations = coefficients.len().div_ceil(decimation);
    if operations * decimation > FIR2_RAM {
        bail!("FIR2DSP coefficients too long for RAM");
    }
    Ok((0..FIR2_RAM)
        .map(|addr| {
            let k = addr / operations;
            if k >= decimation {
                return 0;
            }
            let j = addr % operations;
            let n = j * decimation + (decimation - 1 - k);
            coefficients.get(n).copied().unwrap_or(0)
        })
        .collect())
}

pub fn fir4dsp_fields(coefficients: &[i32], decimation: usize) -> Result<StageFields> {
    let branch_len = coefficients.len().div_ceil(decimation);
    Ok(StageFields {
        decimation: u8::try_from(decimation)?,
        operations_minus_one: u8::try_from(branch_len.div_ceil(2) - 1)?,
        odd_operations: branch_len % 2 == 1,
    })
}

pub fn fir2dsp_fields(coefficients: &[i32], decimation: usize) -> Result<StageFields> {
    Ok(StageFields {
        decimation: u8::try_from(decimation)?,
        operations_minus_one: u8::try_from(coefficients.len().div_ceil(decimation) - 1)?,
        odd_operations: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // p25-httpd's control and chain-1 loaders, verbatim, recording each (address, coefficient)
    // write: the RAM images must reproduce them exactly.
    fn reference_4dsp(coefficients: &[i32], decimation: usize, addr_offset: usize) -> Vec<(usize, i32)> {
        const NUM_ADDR: usize = 256;
        let branch_len = coefficients.len().div_ceil(decimation);
        let operations = branch_len.div_ceil(2);
        let mut out = Vec::new();
        for addr in 0..NUM_ADDR {
            let (off, fold) = if addr >= NUM_ADDR / 2 { (1, NUM_ADDR / 2) } else { (0, 0) };
            let k = (addr - fold) / operations;
            let coeff = if k >= decimation {
                0
            } else {
                let j = (addr - fold) % operations;
                let n = (2 * j + off) * decimation + (decimation - 1 - k);
                *coefficients.get(n).unwrap_or(&0)
            };
            out.push((addr + addr_offset, coeff));
        }
        out
    }

    fn reference_2dsp(coefficients: &[i32], decimation: usize, addr_offset: usize) -> Vec<(usize, i32)> {
        const NUM_ADDR: usize = 128;
        let operations = coefficients.len().div_ceil(decimation);
        let mut out = Vec::new();
        for addr in 0..NUM_ADDR {
            let k = addr / operations;
            let coeff = if k >= decimation {
                0
            } else {
                let j = addr % operations;
                let n = j * decimation + (decimation - 1 - k);
                *coefficients.get(n).unwrap_or(&0)
            };
            out.push((addr + addr_offset, coeff));
        }
        out
    }

    fn placed(ram: &[i32], base: usize) -> Vec<(usize, i32)> {
        ram.iter().enumerate().map(|(a, c)| (a + base, *c)).collect()
    }

    #[test]
    fn ram_images_match_the_p25_httpd_loaders_for_every_preset() {
        for p in PRESETS {
            let load = FirLoad::of(p).unwrap();
            assert_eq!(placed(&load.ram[0], FIR_BASE[0]), reference_4dsp(p.fir1_coeffs, p.decim1, 0), "{} fir1", p.name);
            assert_eq!(placed(&load.ram[1], FIR_BASE[1]), reference_2dsp(p.fir2_coeffs, p.decim2, 256), "{} fir2", p.name);
            assert_eq!(placed(&load.ram[2], FIR_BASE[2]), reference_4dsp(p.fir3_coeffs, p.decim3, 512), "{} fir3", p.name);
        }
    }

    #[test]
    fn stage_fields_match_the_p25_httpd_loaders() {
        for p in PRESETS {
            let f = FirLoad::of(p).unwrap().stages;
            let bl = p.fir1_coeffs.len().div_ceil(p.decim1);
            assert_eq!(f[0].decimation as usize, p.decim1, "{}", p.name);
            assert_eq!(f[0].operations_minus_one as usize, bl.div_ceil(2) - 1, "{}", p.name);
            assert_eq!(f[0].odd_operations, bl % 2 == 1, "{}", p.name);
            assert_eq!(f[1].operations_minus_one as usize, p.fir2_coeffs.len().div_ceil(p.decim2) - 1);
            assert!(!f[1].odd_operations);
        }
    }

    #[test]
    fn oversized_stages_are_rejected() {
        assert!(fir4dsp_ram(&[1; 300], 1).is_err());
        assert!(fir2dsp_ram(&[1; 200], 1).is_err());
    }

    #[test]
    fn every_preset_decimates_to_the_ddc_output_rate() {
        for p in PRESETS {
            assert_eq!(p.sample_rate_hz as usize % p.total_decim(), 0, "{}", p.name);
            assert_eq!(p.sample_rate_hz as usize / p.total_decim(), DDC_OUTPUT_RATE_HZ as usize, "{}", p.name);
        }
        assert_eq!(LSM_INPUT_RATE_HZ, 25_000);
    }
}
