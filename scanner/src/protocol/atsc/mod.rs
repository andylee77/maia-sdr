//! ATSC TV. In the spectrum: the US channel plan (RF channels 2–36), the windows that read it two
//! channels at a time, and what one channel's spectrum says (`spectrum`): an 8-VSB signal
//! (ATSC 1.0, A/53) by its pilot, a wideband signal without that pilot (ATSC 3.0 or another), or
//! nothing. In the signal: the 8-VSB receiver (`demod`, `fec`) and what a station says of itself
//! (`ts`, `psip`), together in `receiver`.

pub mod demod;
pub mod fec;
pub mod psip;
pub mod receiver;
pub mod spectrum;
pub mod ts;
pub mod vsb;

use serde::Serialize;

pub const CHANNEL_HZ: u64 = 6_000_000;
/// 8-VSB's symbol rate: 4.5 MHz × 684 / 286.
pub const SYMBOL_RATE_HZ: f64 = 4_500_000.0 * 684.0 / 286.0;
/// The pilot sits at the lower band edge of the 5.38 MHz Nyquist band, centred in the channel:
/// 309.44 kHz above the channel's lower edge.
pub const PILOT_OFFSET_HZ: f64 = CHANNEL_HZ as f64 / 2.0 - SYMBOL_RATE_HZ / 4.0;
/// The lowest LO the AD9361 tunes.
pub const TUNE_MIN_HZ: u64 = 70_000_000;
/// The LO sits this far above the middle of the channels a window reads: off the channel edges
/// (where the noise floor is read) and off every pilot.
pub const LO_OFFSET_HZ: u64 = 500_000;

/// The bands as the channel plan names them: first channel, last channel, lower edge of the
/// first. VHF low has a gap (72–76 MHz) between channels 4 and 5.
const BANDS: &[(&str, u8, u8, u64)] = &[
    ("VHF low", 2, 4, 54_000_000),
    ("VHF low", 5, 6, 76_000_000),
    ("VHF high", 7, 13, 174_000_000),
    ("UHF", 14, 36, 470_000_000),
];

pub const FIRST: u8 = 2;
pub const LAST: u8 = 36;

/// Runs `f` over the two halves of `out` (split at a whole number of `unit`s) at once, on the
/// A9's two cores (the trunking site sleeps in ATSC mode); `f` is given each half's offset.
pub(crate) fn on_both_cores<T: Send>(out: &mut [T], unit: usize, f: impl Fn(usize, &mut [T]) + Sync) {
    let mid = out.len() / unit / 2 * unit;
    let (a, b) = out.split_at_mut(mid);
    std::thread::scope(|s| {
        s.spawn(|| f(mid, b));
        f(0, a);
    });
}

/// One RF channel of the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Channel {
    pub number: u8,
    pub band: &'static str,
    pub low_hz: u64,
}

impl Channel {
    pub fn get(number: u8) -> Option<Channel> {
        BANDS
            .iter()
            .find(|&&(_, first, last, _)| (first..=last).contains(&number))
            .map(|&(band, first, _, low)| Channel { number, band, low_hz: low + u64::from(number - first) * CHANNEL_HZ })
    }

    pub fn all() -> impl Iterator<Item = Channel> {
        (FIRST..=LAST).filter_map(Channel::get)
    }

    pub fn high_hz(&self) -> u64 {
        self.low_hz + CHANNEL_HZ
    }

    pub fn center_hz(&self) -> u64 {
        self.low_hz + CHANNEL_HZ / 2
    }

    pub fn pilot_hz(&self) -> f64 {
        self.low_hz as f64 + PILOT_OFFSET_HZ
    }

    /// A window can read it: it fits, edges included, within ±`usable_half_hz` of an LO the
    /// AD9361 tunes.
    pub fn reachable(&self, usable_half_hz: f64) -> bool {
        fits(&[*self], lo_for(self.low_hz, self.high_hz()), usable_half_hz)
    }
}

/// One LO position and the channels read there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub lo_hz: u64,
    pub channels: Vec<Channel>,
}

fn lo_for(low_hz: u64, high_hz: u64) -> u64 {
    ((low_hz + high_hz) / 2 + LO_OFFSET_HZ).max(TUNE_MIN_HZ)
}

/// Every channel, with the noise floor read just past its edges, inside ±`usable_half_hz`.
fn fits(channels: &[Channel], lo_hz: u64, usable_half_hz: f64) -> bool {
    channels.iter().all(|c| {
        let edge = spectrum::EDGE_HZ;
        c.low_hz as f64 - edge >= lo_hz as f64 - usable_half_hz && c.high_hz() as f64 + edge <= lo_hz as f64 + usable_half_hz
    })
}

/// The windows that read `numbers` (channels of the plan, in any order): two adjacent channels
/// per window where both fit, else one; channels no window reaches are left out.
pub fn windows(numbers: &[u8], usable_half_hz: f64) -> Vec<Window> {
    let mut chans: Vec<Channel> = numbers.iter().filter_map(|&n| Channel::get(n)).collect();
    chans.sort_by_key(|c| c.number);
    chans.dedup();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chans.len() {
        if let Some(&next) = chans.get(i + 1) {
            let pair = [chans[i], next];
            let lo = lo_for(pair[0].low_hz, next.high_hz());
            if pair[0].high_hz() == next.low_hz && fits(&pair, lo, usable_half_hz) {
                out.push(Window { lo_hz: lo, channels: pair.to_vec() });
                i += 2;
                continue;
            }
        }
        let c = chans[i];
        let lo = lo_for(c.low_hz, c.high_hz());
        if fits(&[c], lo, usable_half_hz) {
            out.push(Window { lo_hz: lo, channels: vec![c] });
        }
        i += 1;
    }
    out
}

#[cfg(test)]
pub(crate) mod tests;
