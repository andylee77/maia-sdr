//! Offline: the video of programmes of unit A's transport streams decoded here and by FFmpeg.
//! `ATSC_PLAYER_DATA` as in `ts_vs_ffmpeg.rs`; `player_ref/video/<capture>_p<programme>.yuv` is
//! FFmpeg's decode of the programme's first frames (yuv420p at the display size, display order)
//! and `.frames` each one's PTS, interlacing and type (`player_ref/video/make.sh`).
//!
//! The streams are followed from their PMT on, and decoding starts at the first I-picture after
//! it; FFmpeg also decodes what came before. From our first frame on, every frame of either
//! decoder must be the other's, by PTS, of the same type and close in every plane (the IDCTs
//! differ within the standard's accuracy, and the difference grows along a GOP's predictions).

use atsc_player::mpeg2::{Decoder, Frame};
use atsc_player::ts::{Demux, STREAM_MPEG2_VIDEO};
use std::collections::HashMap;

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let se: f64 = a.iter().zip(b).map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2)).sum();
    if se == 0.0 {
        return 99.0;
    }
    10.0 * (255.0 * 255.0 * a.len() as f64 / se).log10()
}

/// A plane of `f` at the display size.
fn crop(plane: &[u8], stride: usize, w: usize, h: usize) -> Vec<u8> {
    (0..h).flat_map(|y| plane[y * stride..y * stride + w].iter().copied()).collect()
}

fn decode(ts: &[u8], program: u16, until: u64) -> (Vec<Frame>, u32) {
    let mut d = Demux::default();
    d.follow(Some(program));
    let mut dec = Decoder::default();
    let mut frames = Vec::new();
    let mut pid = None;
    for chunk in ts.chunks(188 * 50) {
        d.push(chunk);
        if pid.is_none() {
            pid = d.programs().find(|p| p.number == program).and_then(|p| p.streams.iter().find(|s| s.stream_type == STREAM_MPEG2_VIDEO).map(|s| s.pid));
        }
        while let Some(pes) = d.next_pes() {
            if Some(pes.pid) == pid {
                dec.push(&pes.data, pes.pts);
            }
        }
        while let Some(f) = dec.next_frame() {
            frames.push(f);
        }
        if frames.last().and_then(|f| f.pts).is_some_and(|t| t > until) {
            break;
        }
    }
    (frames, dec.slice_errors)
}

#[test]
fn frames_match_ffmpeg() {
    let Ok(dir) = std::env::var("ATSC_PLAYER_DATA") else {
        eprintln!("ATSC_PLAYER_DATA not set: skipped");
        return;
    };
    let dir = std::path::Path::new(&dir);
    let vdir = dir.join("player_ref").join("video");
    let mut names: Vec<String> = std::fs::read_dir(&vdir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "frames"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert!(!names.is_empty());
    let mut failures = Vec::new();
    for name in names {
        let (capture, program) = name.rsplit_once("_p").unwrap();
        let ts = std::fs::read(dir.join("ts").join(format!("{capture}.ts"))).unwrap();
        let yuv = std::fs::read(vdir.join(format!("{name}.yuv"))).unwrap();
        let listing = std::fs::read_to_string(vdir.join(format!("{name}.frames"))).unwrap();
        // pts, interlacing, type, size.
        let refs: Vec<(u64, String, String, usize, usize)> = listing
            .lines()
            .map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                let (w, h) = f[3].split_once('x').unwrap();
                (f[0].parse().unwrap(), f[1].to_string(), f[2].to_string(), w.parse().unwrap(), h.parse().unwrap())
            })
            .collect();
        let (w, h) = (refs[0].3, refs[0].4);
        let fsize = w * h * 3 / 2;
        let count = yuv.len() / fsize;
        let by_pts: HashMap<u64, usize> = refs.iter().take(count).enumerate().map(|(i, r)| (r.0, i)).collect();
        let last = refs.iter().take(count).map(|r| r.0).max().unwrap();
        let (frames, errors) = decode(&ts, program.parse().unwrap(), last);
        let mut worst = (99.0f64, 0u64, ' ');
        let mut matched = 0;
        let mut kinds = String::new();
        let first = frames.iter().find_map(|f| f.pts).unwrap_or(u64::MAX);
        let theirs = refs.iter().take(count).filter(|r| r.0 >= first).count();
        let mut unmatched = Vec::new();
        for f in &frames {
            let Some(&i) = f.pts.and_then(|t| by_pts.get(&t)) else {
                if f.pts.is_none_or(|t| t <= last) {
                    unmatched.push(f.pts);
                }
                continue;
            };
            matched += 1;
            kinds.push(f.kind);
            let r = &yuv[i * fsize..(i + 1) * fsize];
            assert_eq!((f.width, f.height), (w, h), "{name}");
            assert_eq!(f.kind.to_string(), refs[i].2, "{name}: frame {i} type");
            let y = psnr(&crop(&f.y, f.stride, w, h), &r[..w * h]);
            let cb = psnr(&crop(&f.cb, f.stride / 2, w / 2, h / 2), &r[w * h..w * h + w * h / 4]);
            let cr = psnr(&crop(&f.cr, f.stride / 2, w / 2, h / 2), &r[w * h + w * h / 4..]);
            let m = y.min(cb).min(cr);
            if m < worst.0 {
                worst = (m, f.pts.unwrap(), f.kind);
            }
        }
        eprintln!(
            "{name}: {w}x{h}, from PTS {first}: {matched} frames, FFmpeg's {theirs} ({kinds}); slice errors {errors}; worst plane {:.1} dB (PTS {}, {})",
            worst.0, worst.1, worst.2
        );
        if matched == 0 || matched != theirs || !unmatched.is_empty() || errors > 0 || worst.0 < 40.0 {
            failures.push(format!("{name}: {matched} matched of FFmpeg's {theirs}, ours not in FFmpeg's {unmatched:?}, worst {:.1} dB", worst.0));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
