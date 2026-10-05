//! Offline: unit A's transport streams through the demultiplexer. `ATSC_PLAYER_DATA` is a
//! capture set's directory: `ts/<capture>.ts` (083's receiver output) and `player_ref/` (see
//! `ac3_vs_ffmpeg.rs`), whose `<capture>_p<programme>.ac3` is FFmpeg's copy of that programme's
//! first AC-3 stream and `.pts` FFmpeg's packets of it (ffprobe's `pts,size` per syncframe).

use atsc_player::ac3::frame_len;
use atsc_player::ts::{Demux, Pes, STREAM_AC3, STREAM_MPEG2_VIDEO};
use std::collections::HashMap;

fn demux_all(ts: &[u8], program: Option<u16>) -> (Demux, Vec<Pes>) {
    let mut d = Demux::default();
    d.follow(program);
    let mut out = Vec::new();
    // In uneven pieces, as a socket would deliver them.
    for chunk in ts.chunks(1000) {
        d.push(chunk);
        while let Some(p) = d.next_pes() {
            out.push(p);
        }
    }
    d.finish();
    while let Some(p) = d.next_pes() {
        out.push(p);
    }
    (d, out)
}

#[test]
fn ac3_streams_and_timestamps_match_ffmpeg() {
    let Ok(dir) = std::env::var("ATSC_PLAYER_DATA") else {
        eprintln!("ATSC_PLAYER_DATA not set: skipped");
        return;
    };
    let dir = std::path::Path::new(&dir);
    let mut names: Vec<String> = std::fs::read_dir(dir.join("player_ref"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "ac3"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert!(!names.is_empty());
    for name in names {
        let (capture, program) = name.rsplit_once("_p").unwrap();
        let program: u16 = program.parse().unwrap();
        let reference = std::fs::read(dir.join("player_ref").join(format!("{name}.ac3"))).unwrap();
        let ts = std::fs::read(dir.join("ts").join(format!("{capture}.ts"))).unwrap();
        let (d, _) = demux_all(&ts, None);
        let prog = d.programs().find(|p| p.number == program).unwrap_or_else(|| panic!("{name}: no PMT for programme {program}")).clone();
        let audio: Vec<u16> = prog.streams.iter().filter(|s| s.stream_type == STREAM_AC3).map(|s| s.pid).collect();
        let video: Vec<u16> = prog.streams.iter().filter(|s| s.stream_type == STREAM_MPEG2_VIDEO).map(|s| s.pid).collect();
        assert_eq!(video.len(), 1, "{name}: {:?}", prog.streams);
        let (_, pes) = demux_all(&ts, Some(program));
        // FFmpeg's copy is the first AC-3 stream of the programme.
        let pid = audio[0];
        let ours: Vec<&Pes> = pes.iter().filter(|p| p.pid == pid).collect();
        let es: Vec<u8> = ours.iter().flat_map(|p| p.data.iter().copied()).collect();
        // The capture's end cuts the last PES packet short; none other may be damaged.
        let damaged: Vec<usize> = ours.iter().enumerate().filter(|(_, p)| p.damaged).map(|(i, _)| i).collect();
        assert!(damaged.iter().all(|&i| i + 1 == ours.len()), "{name}: damaged PES packets {damaged:?} of {}", ours.len());
        // The streams are followed from their PMT on; FFmpeg's demultiplexer also keeps what came
        // before it. From there the two must be the same bytes.
        assert!(reference.ends_with(&es), "{name}: PID {pid:#x}: {} bytes, not the end of FFmpeg's {}", es.len(), reference.len());
        let before_pmt = reference.len() - es.len();
        // Each PES packet's PTS is that of the first syncframe starting in it (2.4.3.7), which is
        // FFmpeg's for the packet at the same place in the stream.
        let mut theirs = HashMap::new();
        let mut place = 0;
        for line in std::fs::read_to_string(dir.join("player_ref").join(format!("{name}.pts"))).unwrap().lines() {
            let f: Vec<&str> = line.split(',').collect();
            theirs.insert(place, f[0].parse::<u64>().ok());
            place += f[1].parse::<usize>().unwrap();
        }
        assert_eq!(place, reference.len());
        let mut starts = Vec::new();
        let mut at = 0;
        while at + 5 <= es.len() {
            match frame_len(&es[at..]) {
                Some(len) => {
                    starts.push(at);
                    at += len;
                }
                None => at += 1,
            }
        }
        let mut offset = 0;
        let mut checked = 0;
        for p in &ours {
            let frame = starts.partition_point(|&s| s < offset);
            offset += p.data.len();
            let (Some(pts), Some(&start)) = (p.pts, starts.get(frame)) else { continue };
            if start >= offset {
                continue;
            }
            assert_eq!(theirs.get(&(before_pmt + start)), Some(&Some(pts)), "{name}: the PES packet with frame {frame}");
            checked += 1;
        }
        let vpes: Vec<&Pes> = pes.iter().filter(|p| p.pid == video[0]).collect();
        let vdamaged = vpes.iter().filter(|p| p.damaged).count();
        eprintln!(
            "{name}: programme {program} streams {:?}; AC-3 PID {pid:#x}: {} PES, {} bytes equal to FFmpeg's ({before_pmt} before the PMT), {checked} PTS checked; video PID {:#x}: {} PES ({vdamaged} damaged)",
            prog.streams,
            ours.len(),
            es.len(),
            video[0],
            vpes.len()
        );
    }
}
