#!/usr/bin/env python3
"""Decode captured IMBE frames via the Fishball vocoder and save as WAV.

Fetches clear IMBE frames from /api/imbe_dump, sends them one at a time
to the vocoder via a temporary Rust test, and saves the resulting PCM
as a WAV file.

Actually simpler: use ctypes to call mbelib directly from Python.

Usage:
    python tools/p25_decode_imbe_capture.py [imbe_clear.hex]
"""

import ctypes
import os
import struct
import sys
import wave


def find_mbelib():
    """Find the compiled mbelib library from the cargo build."""
    candidates = [
        "p25-httpd/target/debug/build/mbelib-sys-*/out/libmbelib.a",
        "p25-httpd/target/debug/libmbelib.lib",
    ]
    # On Windows with MSVC, mbelib is a .lib static library -- can't load
    # with ctypes. We need to build a shared library or use a different approach.
    # Instead, let's just use the Rust binary to decode.
    return None


def decode_with_rust_test(hex_file, wav_file):
    """Write a Rust test that reads hex frames, decodes, and writes WAV."""
    # Read hex frames
    with open(hex_file) as f:
        lines = [l.strip() for l in f if l.strip()]

    print(f"Read {len(lines)} IMBE frames from {hex_file}")

    # Generate Rust test source
    frames_array = ",\n        ".join(
        f'"{line}"' for line in lines[:128]  # cap at 128
    )

    test_code = f'''
    #[test]
    #[ignore]  // run with: cargo test -- --ignored decode_captured
    fn decode_captured_frames_to_wav() {{
        let hex_frames = [
        {frames_array}
        ];

        let mut decoder = super::ImbeDecoder::new();
        let mut all_pcm: Vec<i16> = Vec::new();
        let mut total_errs = 0u64;

        for (i, hex) in hex_frames.iter().enumerate() {{
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|j| u8::from_str_radix(&hex[j..j+2], 16).unwrap())
                .collect();
            let mut bits = [0u8; 18];
            bits.copy_from_slice(&bytes[..18]);
            let frame = crate::p25::voice_frame::ImbeFrameRaw {{ bits }};
            let (pcm, errs, errs2) = decoder.decode_frame(&frame);
            all_pcm.extend_from_slice(&pcm);
            total_errs += errs as u64;
            if i < 9 {{
                let max_sample = pcm.iter().map(|s| s.abs()).max().unwrap_or(0);
                eprintln!("frame {{i}}: errs={{errs}} errs2={{errs2}} max_sample={{max_sample}}");
            }}
        }}

        eprintln!("Decoded {{}} frames, total_errs={{}}, pcm_samples={{}}",
            hex_frames.len(), total_errs, all_pcm.len());

        // Check if audio has any energy
        let max_abs: i16 = all_pcm.iter().map(|s| s.abs()).max().unwrap_or(0);
        let rms = (all_pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>()
            / all_pcm.len() as f64).sqrt();
        eprintln!("max_abs={{max_abs}}, rms={{rms:.1f}}");

        // Write WAV
        let wav_path = "imbe_decoded.wav";
        let mut f = std::fs::File::create(wav_path).unwrap();
        use std::io::Write;
        // WAV header: 8kHz, 16-bit, mono
        let data_size = (all_pcm.len() * 2) as u32;
        let file_size = 36 + data_size;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&file_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();  // PCM
        f.write_all(&1u16.to_le_bytes()).unwrap();  // mono
        f.write_all(&8000u32.to_le_bytes()).unwrap();
        f.write_all(&16000u32.to_le_bytes()).unwrap();  // byte rate
        f.write_all(&2u16.to_le_bytes()).unwrap();  // block align
        f.write_all(&16u16.to_le_bytes()).unwrap();  // bits per sample
        f.write_all(b"data").unwrap();
        f.write_all(&data_size.to_le_bytes()).unwrap();
        for &sample in &all_pcm {{
            f.write_all(&sample.to_le_bytes()).unwrap();
        }}
        eprintln!("Wrote {{wav_path}} ({{}:.1f}s of audio)", all_pcm.len() as f64 / 8000.0);
    }}
'''
    print(test_code)
    print("\nPaste this test into p25-httpd/src/vocoder/mod.rs inside mod tests {}")
    print("Then run: cd p25-httpd && cargo test -- --ignored decode_captured")


def main():
    hex_file = sys.argv[1] if len(sys.argv) >= 2 else "imbe_clear.hex"

    if not os.path.exists(hex_file):
        print(f"File not found: {hex_file}")
        print("Run: python tools/p25_imbe_test.py  to capture frames first")
        return 1

    decode_with_rust_test(hex_file, "imbe_decoded.wav")
    return 0


if __name__ == "__main__":
    sys.exit(main())
