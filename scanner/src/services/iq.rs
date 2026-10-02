//! IQ taps: copies of a receive chain's IQ for the diagnostics. The control decoders hand each
//! chunk in as they get it; a copy is made only while a capture waits. Captures come out as WAV,
//! I left and Q right, the form SDRTrunk's tools and p25-httpd's dumps use.

use std::sync::Mutex;

use tokio::sync::oneshot;

/// The control chain's DDC output rate.
pub const CONTROL_RATE: u32 = 50_000;

struct Capture {
    /// Interleaved values wanted (two per sample).
    want: usize,
    buf: Vec<i16>,
    done: oneshot::Sender<Vec<i16>>,
}

#[derive(Default)]
pub struct IqTap {
    captures: Mutex<Vec<Capture>>,
}

impl IqTap {
    /// Interleaved I, Q as the decoder got it.
    pub fn push(&self, iq: &[i16]) {
        let Ok(mut captures) = self.captures.lock() else { return };
        if captures.is_empty() {
            return;
        }
        for c in captures.iter_mut() {
            let n = (c.want - c.buf.len()).min(iq.len());
            c.buf.extend_from_slice(&iq[..n]);
        }
        let mut i = 0;
        while i < captures.len() {
            if captures[i].buf.len() >= captures[i].want {
                let c = captures.remove(i);
                let _ = c.done.send(c.buf);
            } else {
                i += 1;
            }
        }
    }

    /// The next `samples` complex samples the decoder gets.
    pub fn capture(&self, samples: usize) -> oneshot::Receiver<Vec<i16>> {
        let (tx, rx) = oneshot::channel();
        if let Ok(mut captures) = self.captures.lock() {
            captures.push(Capture { want: samples * 2, buf: Vec::with_capacity(samples * 2), done: tx });
        }
        rx
    }
}

/// Interleaved I, Q as a 16-bit stereo WAV.
pub fn wav_bytes(iq: &[i16], rate: u32) -> Vec<u8> {
    let data_bytes = (iq.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + iq.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(data_bytes + 36).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&2u16.to_le_bytes()); // I, Q
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes()); // block align
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_bytes.to_le_bytes());
    for s in iq {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capture_takes_exactly_what_it_asked_across_chunks() {
        let tap = IqTap::default();
        tap.push(&[9; 10]);
        let mut rx = tap.capture(5);
        tap.push(&[1, 2, 3, 4, 5, 6]);
        assert!(rx.try_recv().is_err(), "three samples of five");
        tap.push(&[7, 8, 9, 10, 11, 12]);
        assert_eq!(rx.try_recv().unwrap(), vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        // Done captures leave the tap: nothing is copied after.
        tap.push(&[0; 4]);
        assert!(tap.captures.lock().unwrap().is_empty());
    }

    #[test]
    fn the_wav_is_stereo_16_bit_at_the_rate() {
        let b = wav_bytes(&[1, -1, 2, -2], CONTROL_RATE);
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(u16::from_le_bytes([b[22], b[23]]), 2);
        assert_eq!(u32::from_le_bytes([b[24], b[25], b[26], b[27]]), CONTROL_RATE);
        assert_eq!(u32::from_le_bytes([b[40], b[41], b[42], b[43]]), 8);
        assert_eq!(b.len(), 44 + 8);
    }
}
