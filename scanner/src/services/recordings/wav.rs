//! Recordings as WAV: 8 kHz 16-bit mono PCM, the vocoder's own output, with the sizes filled in.
//! Bookmarks follow the audio as standard cue points, each with a label and a region (`cue `,
//! then `LIST` `adtl` with `labl` and `ltxt`), which audio editors show as markers.

pub const SAMPLE_RATE: u32 = 8_000;
pub const HEADER_BYTES: u64 = 44;
/// Bytes of audio per millisecond.
const BYTES_PER_MS: u64 = 16;

/// A marked place in the audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub offset_ms: u64,
    pub duration_ms: u64,
    pub label: String,
}

pub fn wav_bytes(pcm: &[i16], marks: &[Mark]) -> Vec<u8> {
    let data_bytes = (pcm.len() * 2) as u32;
    let tail = cue_chunks(marks, pcm.len() as u32);
    let mut b = Vec::with_capacity(HEADER_BYTES as usize + pcm.len() * 2 + tail.len());
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(data_bytes + 36 + tail.len() as u32).to_le_bytes());
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
    b.extend_from_slice(&tail);
    b
}

/// The `cue ` and `LIST` `adtl` chunks of `marks` (none without marks).
fn cue_chunks(marks: &[Mark], samples: u32) -> Vec<u8> {
    if marks.is_empty() {
        return Vec::new();
    }
    let at = |ms: u64| ((ms * u64::from(SAMPLE_RATE) / 1000) as u32).min(samples);
    let mut cue = Vec::new();
    cue.extend_from_slice(&(marks.len() as u32).to_le_bytes());
    let mut adtl = b"adtl".to_vec();
    for (i, m) in marks.iter().enumerate() {
        let id = i as u32 + 1;
        let start = at(m.offset_ms);
        cue.extend_from_slice(&id.to_le_bytes());
        cue.extend_from_slice(&start.to_le_bytes());
        cue.extend_from_slice(b"data");
        cue.extend_from_slice(&0u32.to_le_bytes());
        cue.extend_from_slice(&0u32.to_le_bytes());
        cue.extend_from_slice(&start.to_le_bytes());
        let mut text = id.to_le_bytes().to_vec();
        text.extend_from_slice(m.label.as_bytes());
        text.push(0);
        push_chunk(&mut adtl, b"labl", &text);
        let mut region = id.to_le_bytes().to_vec();
        region.extend_from_slice(&(at(m.offset_ms + m.duration_ms) - start).to_le_bytes());
        region.extend_from_slice(b"rgn ");
        region.extend_from_slice(&[0; 8]); // country, language, dialect, code page
        push_chunk(&mut adtl, b"ltxt", &region);
    }
    let mut out = Vec::new();
    push_chunk(&mut out, b"cue ", &cue);
    push_chunk(&mut out, b"LIST", &adtl);
    out
}

/// A RIFF chunk, padded to an even length.
fn push_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}

/// A recording's length from its file size (a bookmarked file's markers add a few ms; the
/// history keeps its true length).
pub fn duration_ms(file_bytes: u64) -> u64 {
    file_bytes.saturating_sub(HEADER_BYTES) / BYTES_PER_MS
}

/// The length of `samples` of audio.
pub fn samples_ms(samples: usize) -> u64 {
    samples as u64 * 1000 / u64::from(SAMPLE_RATE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_length_agree() {
        let pcm = vec![1i16; 8_000];
        let b = wav_bytes(&pcm, &[]);
        assert_eq!(b.len() as u64, HEADER_BYTES + 16_000);
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 16_000);
        assert_eq!(duration_ms(b.len() as u64), 1_000);
        assert_eq!(samples_ms(pcm.len()), 1_000);
    }

    #[test]
    fn marks_are_cue_points_with_labels_and_regions() {
        let pcm = vec![0i16; 16_000];
        let marks = [Mark { offset_ms: 100, duration_ms: 1_100, label: "warble 806.5/1506.0 Hz".into() }];
        let b = wav_bytes(&pcm, &marks);
        let le = |x: &[u8]| u32::from_le_bytes(x.try_into().unwrap());
        assert_eq!(le(&b[4..8]) as usize + 8, b.len());
        let tail = &b[HEADER_BYTES as usize + 32_000..];
        assert_eq!(&tail[..4], b"cue ");
        assert_eq!((le(&tail[4..8]), le(&tail[8..12])), (4 + 24, 1));
        // The point: id 1 at sample 800 of the data chunk.
        assert_eq!((le(&tail[12..16]), le(&tail[16..20])), (1, 800));
        assert_eq!(&tail[20..24], b"data");
        assert_eq!(le(&tail[32..36]), 800);
        let list = &tail[36..];
        assert_eq!((&list[..4], &list[8..12], &list[12..16]), (&b"LIST"[..], &b"adtl"[..], &b"labl"[..]));
        let text_len = le(&list[16..20]) as usize;
        assert_eq!(&list[24..20 + text_len], b"warble 806.5/1506.0 Hz\0");
        let ltxt = &list[20 + text_len + text_len % 2..];
        assert_eq!(&ltxt[..4], b"ltxt");
        assert_eq!(le(&ltxt[12..16]), 8_800);
        assert_eq!(&ltxt[16..20], b"rgn ");
    }
}
