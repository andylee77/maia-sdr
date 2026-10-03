//! Identities of the real sites the P25 tests use, and a CQPSK signal to demodulate.
#![cfg(test)]

use crate::dsp::fsk4::ideal_phase;

/// Clay County, FL (Harris, LSM simulcast): NAC.
pub const CLAY_NAC: u16 = 0x8A1;

/// Clay County: system ID (NET_STS_BCST).
pub const CLAY_SYSTEM_ID: u16 = 0x8A0;

/// Clay County: the control channel, `Channel(0x0639)` on band 0 (base 851.00625 MHz, 6.25 kHz).
pub const CLAY_CONTROL_FREQ_HZ: u64 = 860_962_500;

/// The Florida statewide WACN (Clay and Duval).
pub const FLORIDA_WACN: u32 = 0xBEE00;

/// A lane's IQ rate.
pub const IQ_RATE_HZ: f64 = 50_000.0;

/// CQPSK at 50 kSPS and 0.2 of full scale (±1.0): each dibit turns the carrier by its ideal
/// phase, root-raised-cosine pulses (alpha 0.2, 8 symbols each side), plus a carrier offset in Hz.
pub fn cqpsk(dibits: &[u8], offset_hz: f64) -> (Vec<f32>, Vec<f32>) {
    use std::f64::consts::PI;
    let sps = IQ_RATE_HZ / 4800.0;
    let rrc = |t: f64| -> f64 {
        let a = 0.2;
        if t.abs() < 1e-9 {
            return 1.0 - a + 4.0 * a / PI;
        }
        if (t.abs() - 1.0 / (4.0 * a)).abs() < 1e-9 {
            return a / 2f64.sqrt() * ((1.0 + 2.0 / PI) * (PI / (4.0 * a)).sin() + (1.0 - 2.0 / PI) * (PI / (4.0 * a)).cos());
        }
        ((PI * t * (1.0 - a)).sin() + 4.0 * a * t * (PI * t * (1.0 + a)).cos()) / (PI * t * (1.0 - (4.0 * a * t).powi(2)))
    };
    let n = (dibits.len() as f64 * sps) as usize;
    let (mut re, mut im) = (vec![0.0f64; n], vec![0.0f64; n]);
    let mut phase = 0.0f64;
    for (k, d) in dibits.iter().enumerate() {
        phase += ideal_phase(*d) as f64;
        let center = k as f64 * sps;
        let lo = (center - 8.0 * sps).max(0.0) as usize;
        let hi = ((center + 8.0 * sps) as usize).min(n);
        for s in lo..hi {
            let p = rrc((s as f64 - center) / sps);
            re[s] += p * phase.cos();
            im[s] += p * phase.sin();
        }
    }
    let mut i = Vec::with_capacity(n);
    let mut q = Vec::with_capacity(n);
    for s in 0..n {
        let rot = 2.0 * PI * offset_hz * s as f64 / IQ_RATE_HZ;
        let (c, sn) = (rot.cos(), rot.sin());
        i.push(((re[s] * c - im[s] * sn) * 0.2) as f32);
        q.push(((re[s] * sn + im[s] * c) * 0.2) as f32);
    }
    (i, q)
}

/// [`cqpsk`] as a lane's IQ ring delivers it: interleaved I, Q in 16 bits.
pub fn cqpsk_i16(dibits: &[u8], offset_hz: f64) -> Vec<i16> {
    let (i, q) = cqpsk(dibits, offset_hz);
    i.iter().zip(&q).flat_map(|(a, b)| [(a * 32768.0) as i16, (b * 32768.0) as i16]).collect()
}
