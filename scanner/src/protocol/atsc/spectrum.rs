//! One channel in the wideband spectrometer's frames, averaged in power: its pilot, its plateau
//! and the noise floor at its edges.
//!
//! - **The pilot.** 8-VSB carries a CW pilot 11.3 dB below the data's power. Summed over the bins
//!   it spreads into, it stands 20.1 dB above a 3.9 kHz bin of the data (16 MSPS in 4096 bins),
//!   whatever the FFT window; noise in the plateau lowers that. The strongest bin within
//!   ±`PILOT_SEARCH_HZ` of where the pilot belongs (stations may offset it) is the pilot when it
//!   stands `PILOT_MIN_DB` above the bins around it.
//! - **The plateau.** The median bin of the channel's middle: the data (8-VSB is flat over
//!   4.76 MHz, ATSC 3.0 over 5.83 MHz) and the noise.
//! - **The floor.** At a channel edge both neighbours' spectra fall to nothing (8-VSB's rolloff
//!   ends at the edge, ATSC 3.0 stops 84 kHz short of it), so the deepest bin within ±`EDGE_HZ` of
//!   either edge is the receiver's noise in this window, or a transmitter's shoulders above it.
//!   The plateau over it is the channel's carrier to noise up to the shoulders (35–45 dB down),
//!   read in one window at one gain: the AGC's choices do not enter it.

use serde::Serialize;

use super::Channel;

/// Bins this close to the LO are its DC spur.
const DC_EXCLUDE_HZ: f64 = 25_000.0;
/// Stations may offset their pilot from where the plan puts it; it is looked for this far out.
pub const PILOT_SEARCH_HZ: f64 = 50_000.0;
/// The noise floor is read this far either side of a channel edge.
pub const EDGE_HZ: f64 = 40_000.0;
/// The plateau is read this far inside the edges (clear of 8-VSB's 620 kHz rolloff).
const PLATEAU_INSET_HZ: f64 = 750_000.0;
/// Bins summed each side of the pilot's peak (the window's main lobe).
const PILOT_HALF_BINS: usize = 2;
/// The pilot over the bins around it.
pub const PILOT_MIN_DB: f32 = 10.0;
/// A plateau this far over the floor holds a signal.
pub const OCCUPIED_DB: f32 = 6.0;
/// Equivalent noise bandwidth of the spectrometer's 4-term Blackman–Harris window, in bins: a bin
/// of noise-like signal reads this many bins' worth of its power.
const ENBW_BINS: f64 = 2.0044;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Kind {
    /// ATSC 1.0: the pilot is there.
    #[serde(rename = "8vsb")]
    Vsb,
    /// A signal fills the channel but has no 8-VSB pilot: ATSC 3.0, or something else.
    #[serde(rename = "no_pilot")]
    NoPilot,
    #[serde(rename = "vacant")]
    Vacant,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    pub kind: Kind,
    /// Where the pilot is (8-VSB only).
    pub pilot_hz: Option<f64>,
    /// The pilot over the plateau, dB (8-VSB only): 20.1 for a clean signal.
    pub pilot_db: Option<f32>,
    /// The plateau over the floor at the edges, dB.
    pub level_db: f32,
    /// The channel's power on the spectrum's dB scale.
    pub power_db: f32,
}

fn db(x: f64) -> f32 {
    if x > 0.0 { (10.0 * x.log10()) as f32 } else { -170.0 }
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[v.len() / 2])
}

/// Measure `ch` in `power` (linear power per bin, DC-centred: bin N/2 at `center_hz`, `span_hz`
/// wide). `None` when the channel and its edges are not all in the frame.
pub fn measure(power: &[f64], center_hz: f64, span_hz: f64, ch: &Channel) -> Option<Measurement> {
    let n = power.len();
    if n == 0 {
        return None;
    }
    let bin_hz = span_hz / n as f64;
    let mid = (n / 2) as f64;
    let freq = |i: usize| center_hz + (i as f64 - mid) * bin_hz;
    // The bins within [a, b], less the DC spur's.
    let bins = |a: f64, b: f64| -> Option<Vec<usize>> {
        let (first, last) = (((a - center_hz) / bin_hz + mid).ceil(), ((b - center_hz) / bin_hz + mid).floor());
        if first < 0.0 || last >= n as f64 || first > last {
            return None;
        }
        Some((first as usize..=last as usize).filter(|&i| (freq(i) - center_hz).abs() > DC_EXCLUDE_HZ).collect())
    };
    let (low, high) = (ch.low_hz as f64, ch.high_hz() as f64);
    let plateau = median(bins(low + PLATEAU_INSET_HZ, high - PLATEAU_INSET_HZ)?.into_iter().map(|i| power[i]).collect())?;
    let mut floor = f64::INFINITY;
    for edge in [low, high] {
        for i in bins(edge - EDGE_HZ, edge + EDGE_HZ)? {
            floor = floor.min(power[i]);
        }
    }
    let total: f64 = bins(low, high)?.into_iter().map(|i| power[i]).sum();
    let level_db = db(plateau) - db(floor);
    let power_db = db(total / ENBW_BINS);

    // The pilot: its lobe summed, less the background under it.
    let pilot = ch.pilot_hz();
    let span = bins(pilot - PILOT_SEARCH_HZ, pilot + PILOT_SEARCH_HZ)?;
    let peak = *span.iter().max_by(|&&a, &&b| power[a].total_cmp(&power[b]))?;
    let lobe: Vec<usize> = span.iter().copied().filter(|&i| i.abs_diff(peak) <= PILOT_HALF_BINS).collect();
    let background = median(span.iter().copied().filter(|&i| i.abs_diff(peak) > PILOT_HALF_BINS).map(|i| power[i]).collect())?;
    let excess: Vec<(usize, f64)> = lobe.iter().map(|&i| (i, (power[i] - background).max(0.0))).collect();
    let pilot_power: f64 = excess.iter().map(|&(_, e)| e).sum();
    let found = db(pilot_power) - db(background) >= PILOT_MIN_DB;
    let kind = if found {
        Kind::Vsb
    } else if level_db >= OCCUPIED_DB {
        Kind::NoPilot
    } else {
        Kind::Vacant
    };
    let pilot_hz = found.then(|| excess.iter().map(|&(i, e)| e * freq(i)).sum::<f64>() / pilot_power);
    Some(Measurement { kind, pilot_hz, pilot_db: found.then(|| db(pilot_power) - db(plateau)), level_db, power_db })
}
