//! Offline: AC-3 elementary streams from unit A's captures decoded here and by FFmpeg.
//! `ATSC_PLAYER_DATA` is a capture set's directory; its `player_ref/` holds, for each stream,
//! `<name>.ac3` (FFmpeg's `-c copy -f ac3` of a programme's first AC-3 stream), `<name>.f32`
//! (FFmpeg's decode of it, interleaved f32 in FFmpeg's channel order) and `<name>.layout`
//! (`rate,channels,layout,bitrate` from ffprobe).
//!
//! The comparison is exact: each long block's coefficients, recovered from both decoders' output
//! by the forward transform (A/52 8.2.3.2; the lapped transform gives back a block's coefficients
//! when it and its neighbours are long), must agree in every bin the encoder sent bits for. Only
//! the bins without bits may differ, since each decoder puts its own dither there (7.3.4).
//!
//! FFmpeg departs from the standard once per stream: at the first block whose channels mix long
//! and short transforms, it rebuilds the previous block's overlap as though its output had been
//! downmixed (in 3/2, R takes C's overlap and C, SL and SR are zeroed), a step in that block's
//! output in four channels where this decoder's is continuous. The two transforms spanning that
//! block are left out.

use atsc_player::ac3::{frame_len, BlockRecord, Decoder, FRAME_SAMPLES};

/// Where each of our channels (A/52 order) is in FFmpeg's interleaving, by FFmpeg's layout.
fn ffmpeg_index(layout: &str, acmod: u8, ours: usize) -> usize {
    match (layout, acmod) {
        // FFmpeg 5.1(side): FL FR FC LFE SL SR; ours L C R SL SR LFE.
        ("5.1(side)", 7) => [0, 2, 1, 4, 5, 3][ours],
        ("stereo", 2) => ours,
        _ => panic!("layout {layout} with acmod {acmod}"),
    }
}

#[test]
fn coefficients_with_bits_match_ffmpeg() {
    let Ok(data) = std::env::var("ATSC_PLAYER_DATA") else {
        eprintln!("ATSC_PLAYER_DATA not set: skipped");
        return;
    };
    let dir = std::path::Path::new(&data).join("player_ref");
    let n = 512usize;
    let w = atsc_player::ac3::window();
    let win: Vec<f64> = (0..n).map(|i| f64::from(if i < 256 { w[i] } else { w[511 - i] })).collect();
    let kernel: Vec<f64> = (0..n * 256)
        .map(|j| {
            let (i, k) = (j / 256, j % 256);
            (2.0 * std::f64::consts::PI / (4.0 * n as f64) * (2 * i + 1) as f64 * (2 * k + 1) as f64 + std::f64::consts::PI / 4.0 * (2 * k + 1) as f64).cos()
        })
        .collect();
    let mdct = |seg: &[f32]| -> Vec<f64> {
        let x: Vec<f64> = seg.iter().zip(&win).map(|(&s, &w)| f64::from(s) * w).collect();
        (0..256).map(|k| (0..n).map(|i| x[i] * kernel[i * 256 + k]).sum()).collect()
    };
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "ac3"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert!(!names.is_empty());
    let mut failures = Vec::new();
    for name in names {
        let base = std::path::Path::new(&dir).join(&name);
        let es = std::fs::read(base.with_extension("ac3")).unwrap();
        let reference: Vec<f32> = std::fs::read(base.with_extension("f32")).unwrap().chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let layout_line = std::fs::read_to_string(base.with_extension("layout")).unwrap();
        let fields: Vec<&str> = layout_line.trim().split(',').collect();
        let (nref, layout) = (fields[1].parse::<usize>().unwrap(), fields[2]);
        let mut dec = Decoder::default();
        dec.record = true;
        // Ours and the reference per channel (ours' order), and each block's record.
        let mut ours: Vec<Vec<f32>> = Vec::new();
        let mut theirs: Vec<Vec<f32>> = Vec::new();
        let mut recs: Vec<Vec<BlockRecord>> = Vec::new();
        let mut pos = 0;
        let mut frame = 0;
        let mut nfchans = 0;
        // The stream may start inside a frame.
        while pos + 5 <= es.len() && frame_len(&es[pos..]).is_none() {
            pos += 1;
        }
        while pos + 5 <= es.len() {
            let len = frame_len(&es[pos..]).unwrap_or_else(|| panic!("{name}: no sync word at byte {pos}"));
            if pos + len > es.len() {
                break;
            }
            let f = dec.decode(&es[pos..pos + len]).unwrap_or_else(|e| panic!("{name} frame {frame}: {e:?}"));
            let start = frame * FRAME_SAMPLES * nref;
            assert!(start + FRAME_SAMPLES * nref <= reference.len(), "{name}: FFmpeg decoded fewer frames");
            nfchans = f.pcm.len() - usize::from(f.lfe);
            if ours.is_empty() {
                ours = vec![Vec::new(); f.pcm.len()];
                theirs = vec![Vec::new(); f.pcm.len()];
                recs = vec![Vec::new(); f.pcm.len()];
            }
            for ch in 0..f.pcm.len() {
                let k = ffmpeg_index(layout, f.acmod, ch);
                ours[ch].extend_from_slice(&f.pcm[ch]);
                theirs[ch].extend((0..FRAME_SAMPLES).map(|i| reference[start + i * nref + k]));
                recs[ch].extend(f.record[ch].iter().cloned());
            }
            frame += 1;
            pos += len;
        }
        // FFmpeg's rebuilt overlap: the first block mixing transform lengths, and the two
        // transforms that span its output.
        let nblk = recs[0].len();
        let mixed = (0..nblk).find(|&g| (1..nfchans).any(|ch| recs[ch][g].short != recs[0][g].short));
        let mut report = Vec::new();
        for ch in 0..ours.len() {
            let (mut sig, mut err, mut blocks) = (0f64, 0f64, 0);
            let mut worst: Vec<(f64, usize)> = Vec::new();
            for g in 1..nblk.saturating_sub(1) {
                if recs[ch][g - 1].short || recs[ch][g].short || recs[ch][g + 1].short {
                    continue;
                }
                if mixed.is_some_and(|m| g + 1 == m || g == m) {
                    continue;
                }
                let (a, b) = (mdct(&ours[ch][256 * g..256 * g + n]), mdct(&theirs[ch][256 * g..256 * g + n]));
                let (mut s, mut e) = (0f64, 0f64);
                for k in (0..256).filter(|&k| !recs[ch][g].noise[k]) {
                    s += a[k] * a[k];
                    e += (a[k] - b[k]) * (a[k] - b[k]);
                }
                sig += s;
                err += e;
                worst.push((e / s.max(1e-30), g));
                blocks += 1;
            }
            let snr = 10.0 * (sig / err.max(1e-30)).log10();
            report.push(format!("{snr:.1} dB over {blocks} blocks"));
            if blocks == 0 || snr <= 60.0 {
                worst.sort_by(|x, y| y.0.total_cmp(&x.0));
                let at: Vec<String> = worst.iter().take(3).map(|(r, g)| format!("frame {} block {} ({r:.1e})", g / 6, g % 6)).collect();
                failures.push(format!("{name} channel {ch}: {snr:.1} dB over {blocks} blocks; worst {at:?}"));
            }
        }
        eprintln!("{name}: {frame} frames, FFmpeg's first mixed block {mixed:?}; bins with bits, per channel: {report:?}");
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
