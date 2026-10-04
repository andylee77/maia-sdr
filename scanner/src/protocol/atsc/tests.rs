use super::spectrum::{measure, Kind};
use super::*;

const RATE: f64 = 16e6;
const N: usize = 4096;
const BIN: f64 = RATE / N as f64;
/// The usable half-window at 16 MSPS.
const USABLE: f64 = 7.2e6;
/// Pilot over a 3.9 kHz bin of the data: (1.25² / 21) of the data's power in one bin of 5.38 MHz.
const PILOT_OVER_BIN: f64 = 1.5625 / 21.0 * (SYMBOL_RATE_HZ / 2.0) / BIN;

/// A frame of receiver noise (1.0 a bin) around `lo`.
pub(crate) fn noise() -> Vec<f64> {
    vec![1.0; N]
}

fn pos(lo: f64, f: f64) -> f64 {
    (f - lo) / BIN + (N / 2) as f64
}

/// 8-VSB on `ch` at `cn_db` over the noise, every frequency moved by `shift_hz`: the flat top,
/// the 620 kHz raised-cosine edges and the pilot spread over the Blackman–Harris main lobe. What
/// falls outside the frame is left out.
pub(crate) fn add_vsb(p: &mut [f64], lo: f64, ch: Channel, cn_db: f64, shift_hz: f64) {
    let data = 10f64.powf(cn_db / 10.0);
    let (low, high) = (ch.low_hz as f64 + shift_hz, ch.high_hz() as f64 + shift_hz);
    for (i, x) in p.iter_mut().enumerate() {
        let f = lo + (i as f64 - (N / 2) as f64) * BIN;
        let d = (f - low).min(high - f);
        if d > 0.0 {
            *x += data * if d >= 620e3 { 1.0 } else { (std::f64::consts::PI * d / 1.24e6).sin().powi(2) };
        }
    }
    let x = pos(lo, ch.pilot_hz() + shift_hz);
    if x < 2.0 || x >= (N - 3) as f64 {
        return;
    }
    let lobe = [0.0388, 0.463, 1.0, 0.463, 0.0388];
    let (k, frac) = (x.floor() as usize, x - x.floor());
    for (j, w) in lobe.iter().enumerate() {
        let share = data * PILOT_OVER_BIN * w / 2.0036;
        p[k + j - 2] += share * (1.0 - frac);
        p[k + j - 1] += share * frac;
    }
}

/// ATSC 3.0 on `ch`: flat over the middle 5.83 MHz, no pilot.
pub(crate) fn add_flat(p: &mut [f64], lo: f64, ch: Channel, cn_db: f64) {
    let half = 2.916e6;
    let c = ch.center_hz() as f64;
    let (first, last) = (pos(lo, c - half).ceil().max(0.0), pos(lo, c + half).floor().min((N - 1) as f64));
    if last < first {
        return;
    }
    for x in p.iter_mut().take(last as usize + 1).skip(first as usize) {
        *x += 10f64.powf(cn_db / 10.0);
    }
}

fn ch(n: u8) -> Channel {
    Channel::get(n).unwrap()
}

#[test]
fn the_plan_puts_channels_where_the_fcc_does() {
    assert_eq!(ch(2).low_hz, 54_000_000);
    assert_eq!(ch(4).low_hz, 66_000_000);
    assert_eq!(ch(5).low_hz, 76_000_000, "the 72-76 MHz gap");
    assert_eq!(ch(7).low_hz, 174_000_000);
    assert_eq!(ch(13).high_hz(), 216_000_000);
    assert_eq!(ch(14).low_hz, 470_000_000);
    // The HDHomeRun names channels by their centre: WJXT and WCWJ on RF 20 at 509 MHz.
    assert_eq!(ch(20).center_hz(), 509_000_000);
    assert_eq!(ch(36).high_hz(), 608_000_000);
    assert!(Channel::get(1).is_none() && Channel::get(37).is_none());
    assert!((PILOT_OFFSET_HZ - 309_440.56).abs() < 0.01, "{PILOT_OFFSET_HZ}");
    assert_eq!(Channel::all().count(), 35);
    assert_eq!(ch(9).band, "VHF high");
}

#[test]
fn a_window_reads_two_adjacent_channels_where_they_fit() {
    let all: Vec<u8> = (FIRST..=LAST).rev().chain([14, 20]).collect();
    let w = windows(&all, USABLE);
    let read: Vec<u8> = w.iter().flat_map(|w| w.channels.iter().map(|c| c.number)).collect();
    assert_eq!(read, (4..=36).collect::<Vec<u8>>(), "each once, in order; 2 and 3 are below the AD9361");
    assert_eq!(w.len(), 18);
    // Channel 4 alone at the lowest LO; 4 and 5 are not adjacent (72-76 MHz).
    assert_eq!((w[0].lo_hz, w[0].channels.len()), (70_000_000, 1));
    assert_eq!((w[1].lo_hz, w[1].channels.len()), (82_500_000, 2));
    // UHF: 14 and 15 with the LO 0.5 MHz above their shared edge; 36 alone.
    let uhf = w.iter().position(|w| w.channels[0].number == 14).unwrap();
    assert_eq!(w[uhf].lo_hz, 476_500_000);
    assert_eq!(w.last().unwrap().lo_hz, 605_500_000);
    assert!(!ch(3).reachable(USABLE) && ch(4).reachable(USABLE) && ch(36).reachable(USABLE));
    // A narrower window reads one channel at a time.
    assert!(windows(&[14, 15], 5e6).iter().all(|w| w.channels.len() == 1));
}

#[test]
fn an_8vsb_channel_shows_its_pilot_and_its_carrier_to_noise() {
    let lo = 476_500_000.0;
    let mut p = noise();
    add_vsb(&mut p, lo, ch(14), 30.0, 0.0);
    // A station with its pilot 10.1 kHz high.
    add_vsb(&mut p, lo, ch(15), 20.0, 10_100.0);
    // The LO's DC spur, inside channel 15.
    p[N / 2] = 1e7;
    let m14 = measure(&p, lo, RATE, &ch(14)).unwrap();
    assert_eq!(m14.kind, Kind::Vsb);
    assert!((m14.pilot_db.unwrap() - 20.1).abs() < 0.5, "{m14:?}");
    assert!((m14.level_db - 30.0).abs() < 0.5, "{m14:?}");
    assert!((m14.pilot_hz.unwrap() - ch(14).pilot_hz()).abs() < 300.0, "{m14:?}");
    let m15 = measure(&p, lo, RATE, &ch(15)).unwrap();
    assert_eq!(m15.kind, Kind::Vsb);
    assert!((m15.level_db - 20.0).abs() < 0.5, "{m15:?}");
    assert!((m15.pilot_hz.unwrap() - ch(15).pilot_hz() - 10_100.0).abs() < 300.0, "{m15:?}");
    // The stronger channel has the more power.
    assert!((m14.power_db - m15.power_db - 10.0).abs() < 0.5, "{m14:?} {m15:?}");
}

#[test]
fn atsc_3_fills_its_channel_without_a_pilot() {
    // RF 18 (the Jacksonville lighthouse) and RF 19 (8-VSB) share a window.
    let lo = 500_500_000.0;
    let mut p = noise();
    add_flat(&mut p, lo, ch(18), 25.0);
    add_vsb(&mut p, lo, ch(19), 25.0, 0.0);
    let m18 = measure(&p, lo, RATE, &ch(18)).unwrap();
    assert_eq!((m18.kind, m18.pilot_hz), (Kind::NoPilot, None), "{m18:?}");
    assert!((m18.level_db - 25.0).abs() < 0.5, "{m18:?}");
    assert_eq!(measure(&p, lo, RATE, &ch(19)).unwrap().kind, Kind::Vsb);
}

#[test]
fn a_vacant_channel_beside_strong_ones_is_vacant() {
    // RF 25 is empty between RF 24 and RF 26, both 40 dB over the noise.
    let lo = 536_500_000.0;
    let mut p = noise();
    add_vsb(&mut p, lo, ch(24), 40.0, 0.0);
    add_vsb(&mut p, lo, ch(26), 40.0, 0.0);
    let m = measure(&p, lo, RATE, &ch(25)).unwrap();
    assert_eq!(m.kind, Kind::Vacant, "{m:?}");
    assert!(m.level_db.abs() < 1.0, "{m:?}");
    assert!((measure(&p, lo, RATE, &ch(24)).unwrap().level_db - 40.0).abs() < 0.5);
}

#[test]
fn a_station_below_the_noise_is_found_by_its_pilot() {
    let lo = 476_500_000.0;
    let mut p = noise();
    add_vsb(&mut p, lo, ch(14), -5.0, 0.0);
    let m = measure(&p, lo, RATE, &ch(14)).unwrap();
    assert_eq!(m.kind, Kind::Vsb, "{m:?}");
    assert!(m.level_db < 2.0, "{m:?}");
    // Below about -10 dB the pilot is lost in the noise.
    let mut p = noise();
    add_vsb(&mut p, lo, ch(14), -13.0, 0.0);
    assert_eq!(measure(&p, lo, RATE, &ch(14)).unwrap().kind, Kind::Vacant);
}

#[test]
fn a_channel_outside_the_frame_is_not_measured() {
    assert!(measure(&noise(), 476_500_000.0, RATE, &ch(17)).is_none());
    assert!(measure(&[], 476_500_000.0, RATE, &ch(14)).is_none());
}
