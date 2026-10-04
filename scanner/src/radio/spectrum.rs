//! The spectrometer's frames: every frame the radio core writes, read in order and checked by
//! one task (`run`, every 20 ms; the ring holds 16 frames); the newest kept, its header bins
//! blanked, for whoever asks next (the survey, the crystal tracker, the scans, the API), and the
//! counts that show whether any was missed. A tap sees every frame, for diagnostics.

use std::sync::Mutex;

use serde::Serialize;

use crate::hardware::radiocore::frame::{self, Fault, FrameHeader};

/// A frame of the spectrometer, its header bins blanked.
#[derive(Debug, Clone, PartialEq)]
pub struct SpectrumFrame {
    pub header: FrameHeader,
    /// 4096 little-endian 64-bit bins from -fs/2 (`frame` gives their format).
    pub bytes: Vec<u8>,
}

/// The reader's counts since boot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SpectrumCounters {
    /// Frames read and checked.
    pub frames: u64,
    /// Frames missing between two read (the ring lapped the reader).
    pub missed: u64,
    /// Frames that failed their checks (never written, or stale in the cache).
    pub faults: u64,
    /// The newest frame's header.
    pub last: Option<FrameHeader>,
}

/// Sees every checked frame, header bins included.
pub type Tap = Box<dyn FnMut(&FrameHeader, &[u8]) + Send>;

#[derive(Default)]
struct State {
    latest: Option<SpectrumFrame>,
    counters: SpectrumCounters,
    tap: Option<Tap>,
}

#[derive(Default)]
pub struct Spectrum {
    state: Mutex<State>,
}

impl Spectrum {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The newest frame not yet taken.
    pub fn take(&self) -> Option<SpectrumFrame> {
        self.state().latest.take()
    }

    pub fn counters(&self) -> SpectrumCounters {
        self.state().counters
    }

    /// Hand every frame to `tap` from now on (`None`: no more).
    pub fn set_tap(&self, tap: Option<Tap>) {
        self.state().tap = tap;
    }

    /// One frame as the ring gave it; `newest` keeps it for the next `take`.
    pub fn deliver(&self, frame: Result<(FrameHeader, &[u8]), Fault>, newest: bool) {
        let mut s = self.state();
        let (header, bytes) = match frame {
            Ok(f) => f,
            Err(fault) => {
                s.counters.faults += 1;
                if s.counters.faults <= 20 {
                    tracing::warn!("spectrum frame refused: {fault:?}");
                }
                return;
            }
        };
        let c = &mut s.counters;
        if let Some(prev) = c.last.map(|h| h.sequence) {
            match header.sequence.wrapping_sub(prev) {
                0 => {
                    // The same frame again: a sub-buffer read before the core rewrote it.
                    c.faults += 1;
                    return;
                }
                step if step < 1 << 31 => c.missed += u64::from(step - 1),
                _ => tracing::warn!("spectrum frame sequence went back from {prev} to {}", header.sequence),
            }
        }
        c.frames += 1;
        c.last = Some(header);
        if let Some(tap) = s.tap.as_mut() {
            tap(&header, bytes);
        }
        if newest {
            let mut bytes = bytes.to_vec();
            frame::blank(&mut bytes);
            s.latest = Some(SpectrumFrame { header, bytes });
        }
    }
}

/// Reads the ring until the process ends.
#[cfg(target_os = "linux")]
pub async fn run(core: std::sync::Arc<tokio::sync::Mutex<crate::hardware::radiocore::RadioCore>>, spectrum: std::sync::Arc<Spectrum>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        core.lock().await.read_frames(|f, newest| spectrum.deliver(f.map(|f| (f.header, f.bytes)), newest));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::hardware::radiocore::frame::{build, BINS};

    fn frame(sequence: u32) -> Vec<u8> {
        let h = FrameHeader { sequence, transforms: 256, sample_index: 1000 * sequence as u64, ..Default::default() };
        build(&h, &(0..BINS as u64).collect::<Vec<_>>())
    }

    fn give(s: &Spectrum, bytes: &[u8], newest: bool) {
        s.deliver(frame::parse(bytes).map(|h| (h, bytes)), newest);
    }

    #[test]
    fn gaps_in_the_sequence_are_counted() {
        let s = Spectrum::default();
        for seq in [5, 6, 7, 10, 11] {
            give(&s, &frame(seq), seq == 11);
        }
        let c = s.counters();
        assert_eq!((c.frames, c.missed, c.faults), (5, 2, 0));
        assert_eq!(c.last.unwrap().sequence, 11);
    }

    #[test]
    fn the_newest_frame_is_kept_blanked_and_taken_once() {
        let s = Spectrum::default();
        give(&s, &frame(1), false);
        give(&s, &frame(2), true);
        let f = s.take().expect("the newest");
        assert_eq!(f.header.sequence, 2);
        assert_eq!(f.bytes[..8], 8u64.to_le_bytes());
        assert!(s.take().is_none());
    }

    #[test]
    fn a_frame_read_twice_or_failing_its_check_is_a_fault() {
        let s = Spectrum::default();
        give(&s, &frame(3), false);
        give(&s, &frame(3), false);
        let mut bad = frame(4);
        bad[100] ^= 1;
        give(&s, &bad, true);
        let c = s.counters();
        assert_eq!((c.frames, c.faults, c.missed), (1, 2, 0));
        assert!(s.take().is_none());
    }

    #[test]
    fn the_tap_sees_every_frame() {
        let s = Spectrum::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let into = seen.clone();
        s.set_tap(Some(Box::new(move |h, bytes| into.lock().unwrap().push((h.sequence, bytes.len())))));
        for seq in 1..=3 {
            give(&s, &frame(seq), false);
        }
        assert_eq!(*seen.lock().unwrap(), vec![(1, 32768), (2, 32768), (3, 32768)]);
    }
}
