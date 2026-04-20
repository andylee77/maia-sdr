//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.

use super::*;

/// Decode captured frames with JMBE and save WAV for comparison.
#[test]
#[ignore]
fn decode_captured_jmbe_wav() {
    let hex_frames = [
        "ee54972c2201c44dfa51099dbdf3dd0809a7",
        "c03d1bc0e1008453cef445fbfecc46f692cb",
        "0eb3b6c8868795c5589e4bbac728b235157a",
        "9a9601d6b30bc993822c104af2c5686f495e",
        "0af5d3db801a2b9bffe9106d9ba2a6ca1f60",
        "aaf9f33c9b0aca37953944044436a7aa8bfc",
        "3f2935f65944c1c27d33348249c30884ef34",
        "f4c5e32e4a4ea70cb75164ff46980b443e47",
        "ea20a65ebd0aed61ad8db1b391b8a3a40fe7",
        "c9ef78a76a02d787ad0264fe94bd78a0af23",
        "3fbc77bf8bab6535bbb2629821fcf22a0260",
        "2b09b8294e6da347b9bea8e058c24afc94ee",
        "290a07148ac7f8d79f0fcde678b789a4b408",
        "590d53e371d98fb0f34460f6a9ddc562530f",
        "c6c1935a6145c095f9c129a0e349d56e11f0",
        "040a41803406c1c2b4fefe74edc6f67d13fb",
        "5c2113f867a789fbff4938da2219a6bb9bdc",
        "6e5b33e01062cec70994135ad4186df286ef",
        "2baf0d6c5419b231fabbfdbc1e47b5fa8308",
        "4fa9a2281a03f2ea985c2cd892e086ad236a",
        "395c09f24207aadb9a46e6844555385b781d",
        "398cc1202a07ab31805bd0112bfeb6bc8fa3",
        "4b6fb65b7d21ac9feb0ec05619c5c99aaae1",
        "7dce4c42606ac4850a2034f336b8bee2c68a",
        "093bb3d74cea3b36a4af350c85fb78c51ec4",
        "7d7c54dad6db4940034d6601b79a0ef5239f",
        "6d68557b873a3bc4318c182d5b384a39455f",
    ];

    let mut decoder = JmbeDecoder::new();
    let mut all_pcm: Vec<i16> = Vec::new();

    for (i, hex) in hex_frames.iter().enumerate() {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|j| u8::from_str_radix(&hex[j..j + 2], 16).unwrap())
            .collect();
        let mut bits = [0u8; 18];
        bits.copy_from_slice(&bytes[..18]);
        let frame = ImbeFrameRaw { bits };
        let pcm = decoder.decode_frame(&frame);
        let max_s = pcm.iter().map(|s| s.abs()).max().unwrap_or(0);
        let rms = (pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>()
            / pcm.len() as f64).sqrt();
        eprintln!("JMBE frame {i:2}: max={max_s:6} rms={rms:7.1}");
        all_pcm.extend_from_slice(&pcm);
    }

    let max_abs = all_pcm.iter().map(|s| s.abs()).max().unwrap_or(0);
    let rms = (all_pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>()
        / all_pcm.len() as f64).sqrt();
    eprintln!("\nJMBE Total: {} frames, max={max_abs}, rms={rms:.1}",
        hex_frames.len());

    // Write WAV
    use std::io::Write;
    let wav_path = "imbe_decoded_jmbe.wav";
    let mut f = std::fs::File::create(wav_path).unwrap();
    let data_size = (all_pcm.len() * 2) as u32;
    let file_size = 36 + data_size;
    f.write_all(b"RIFF").unwrap();
    f.write_all(&file_size.to_le_bytes()).unwrap();
    f.write_all(b"WAVE").unwrap();
    f.write_all(b"fmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&8000u32.to_le_bytes()).unwrap();
    f.write_all(&16000u32.to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_size.to_le_bytes()).unwrap();
    for &sample in &all_pcm {
        f.write_all(&sample.to_le_bytes()).unwrap();
    }
    eprintln!("Wrote {wav_path} ({:.1}s)", all_pcm.len() as f64 / 8000.0);
}
