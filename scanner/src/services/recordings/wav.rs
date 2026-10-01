//! Recordings as WAV: 8 kHz 16-bit mono PCM, the vocoder's own output, with the sizes filled in.

pub const SAMPLE_RATE: u32 = 8_000;
pub const HEADER_BYTES: u64 = 44;
/// Bytes of audio per millisecond.
const BYTES_PER_MS: u64 = 16;

pub fn wav_bytes(pcm: &[i16]) -> Vec<u8> {
    let data_bytes = (pcm.len() * 2) as u32;
    let mut b = Vec::with_capacity(HEADER_BYTES as usize + pcm.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(data_bytes + 36).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    b.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes()); // block align
    b.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_bytes.to_le_bytes());
    for s in pcm {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

/// A recording's length from its file size.
pub fn duration_ms(file_bytes: u64) -> u64 {
    file_bytes.saturating_sub(HEADER_BYTES) / BYTES_PER_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_length_agree() {
        let pcm = vec![1i16; 8_000];
        let b = wav_bytes(&pcm);
        assert_eq!(b.len() as u64, HEADER_BYTES + 16_000);
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 16_000);
        assert_eq!(duration_ms(b.len() as u64), 1_000);
    }
}
