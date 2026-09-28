//! Change 066: DDC FIR coefficient RAM images (portable, host-tested).
//!
//! The P25 DDC's three polyphase FIR stages share one coefficient RAM:
//! FIR1 (FIR4DSP, folded) at 0-255, FIR2 (FIR2DSP) at 256-383 and FIR3
//! (FIR4DSP) at 512-767. These functions compute what `fpga.rs` writes
//! at each address, plus the per-stage decimation / operations fields.
//! The chain-2 DDC loader uses them; the control and chain-1 loaders
//! keep their inline loops, which the tests below reproduce exactly.

use anyhow::{bail, Result};

/// FIR4DSP coefficient RAM depth (two folded halves of 128).
pub const FIR4_RAM: usize = 256;
/// FIR2DSP coefficient RAM depth.
pub const FIR2_RAM: usize = 128;
/// RAM base address of each stage.
pub const FIR_BASE: [usize; 3] = [0, 256, 512];

/// Per-stage fields of `ddc_decimation` / `ddc_control`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageFields {
    pub decimation: u8,
    pub operations_minus_one: u8,
    /// FIR4DSP stages only (FIR2DSP has no fold).
    pub odd_operations: bool,
}

/// RAM image of a folded FIR4DSP stage (FIR1, FIR3).
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
    let operations = branch_len.div_ceil(2);
    Ok(StageFields {
        decimation: u8::try_from(decimation)?,
        operations_minus_one: u8::try_from(operations - 1)?,
        odd_operations: branch_len % 2 == 1,
    })
}

pub fn fir2dsp_fields(coefficients: &[i32], decimation: usize) -> Result<StageFields> {
    let operations = coefficients.len().div_ceil(decimation);
    Ok(StageFields {
        decimation: u8::try_from(decimation)?,
        operations_minus_one: u8::try_from(operations - 1)?,
        odd_operations: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::ddc_presets::PRESETS;

    // Verbatim copies of the `fpga.rs` loops (load_fir_4dsp /
    // load_fir_2dsp), recording each (address, coefficient) write.
    fn reference_4dsp(coefficients: &[i32], decimation: usize, addr_offset: usize) -> Vec<(usize, i32)> {
        const NUM_ADDR: usize = 256;
        let branch_len = coefficients.len().div_ceil(decimation);
        let operations = branch_len.div_ceil(2);
        assert!(operations * decimation <= NUM_ADDR / 2);
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
        assert!(operations * decimation <= NUM_ADDR);
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

    fn placed(ram: Vec<i32>, base: usize) -> Vec<(usize, i32)> {
        ram.into_iter().enumerate().map(|(a, c)| (a + base, c)).collect()
    }

    #[test]
    fn ram_images_match_the_chain_one_loader_for_every_preset() {
        for p in PRESETS {
            assert_eq!(
                placed(fir4dsp_ram(p.fir1_coeffs, p.decim1).unwrap(), FIR_BASE[0]),
                reference_4dsp(p.fir1_coeffs, p.decim1, 0),
                "{} fir1", p.name,
            );
            assert_eq!(
                placed(fir2dsp_ram(p.fir2_coeffs, p.decim2).unwrap(), FIR_BASE[1]),
                reference_2dsp(p.fir2_coeffs, p.decim2, 256),
                "{} fir2", p.name,
            );
            assert_eq!(
                placed(fir4dsp_ram(p.fir3_coeffs, p.decim3).unwrap(), FIR_BASE[2]),
                reference_4dsp(p.fir3_coeffs, p.decim3, 512),
                "{} fir3", p.name,
            );
        }
    }

    #[test]
    fn stage_fields_match_the_chain_one_loader() {
        for p in PRESETS {
            let f1 = fir4dsp_fields(p.fir1_coeffs, p.decim1).unwrap();
            let bl = p.fir1_coeffs.len().div_ceil(p.decim1);
            assert_eq!(f1.decimation as usize, p.decim1, "{}", p.name);
            assert_eq!(f1.operations_minus_one as usize, bl.div_ceil(2) - 1, "{}", p.name);
            assert_eq!(f1.odd_operations, bl % 2 == 1, "{}", p.name);
            let f2 = fir2dsp_fields(p.fir2_coeffs, p.decim2).unwrap();
            assert_eq!(f2.operations_minus_one as usize, p.fir2_coeffs.len().div_ceil(p.decim2) - 1);
            assert!(!f2.odd_operations);
        }
    }

    #[test]
    fn oversized_stages_are_rejected() {
        assert!(fir4dsp_ram(&[1; 300], 1).is_err());
        assert!(fir2dsp_ram(&[1; 200], 1).is_err());
    }
}
