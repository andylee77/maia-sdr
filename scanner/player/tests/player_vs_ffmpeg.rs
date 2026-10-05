//! Offline: a programme of each of unit A's transport streams through the player, fed in uneven
//! pieces as a socket delivers them. `ATSC_PLAYER_DATA` as in `ts_vs_ffmpeg.rs`.

use atsc_player::player::{describe, Player};

#[test]
fn every_syncframe_plays_at_ffmpeg_s_time() {
    let Ok(dir) = std::env::var("ATSC_PLAYER_DATA") else {
        eprintln!("ATSC_PLAYER_DATA not set: skipped");
        return;
    };
    let dir = std::path::Path::new(&dir);
    let mut names: Vec<String> = std::fs::read_dir(dir.join("player_ref"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "pts"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert!(!names.is_empty());
    for name in names {
        let (capture, program) = name.rsplit_once("_p").unwrap();
        let ts = std::fs::read(dir.join("ts").join(format!("{capture}.ts"))).unwrap();
        // FFmpeg's time for each syncframe of the stream (its parser's packets).
        let theirs: Vec<Option<u64>> = std::fs::read_to_string(dir.join("player_ref").join(format!("{name}.pts")))
            .unwrap()
            .lines()
            .map(|l| l.split(',').next().unwrap().parse().ok())
            .collect();
        let mut player = Player::new(Some(program.parse().unwrap()));
        let mut ours = Vec::new();
        let mut lost = 0;
        for chunk in ts.chunks(1316 + 7) {
            player.push(chunk);
            while let Some(s) = player.sound() {
                assert_eq!(s.samples.len(), 2 * 1536);
                lost += usize::from(s.lost);
                ours.push(s.pts);
            }
        }
        assert_eq!(lost, 0, "{name}");
        // Ours start where the PMT let the player in; from there each is FFmpeg's.
        let first = theirs.iter().position(|&t| t == ours[0]).unwrap_or_else(|| panic!("{name}: our first time {:?} is not FFmpeg's", ours[0]));
        assert!(theirs.len() >= first + ours.len(), "{name}: {} frames from {first} against FFmpeg's {}", ours.len(), theirs.len());
        assert!(theirs[first..first + ours.len()] == ours[..], "{name}: the times differ");
        eprintln!("{name}: {} syncframes at FFmpeg's times (its {} from its {first}); {}", ours.len(), theirs.len(), describe(&player));
    }
}
