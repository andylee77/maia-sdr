# 084 — The ATSC player: MPEG-2 video and AC-3 decoders in Rust (WebAssembly), and a stream for VLC

**Date:** 2026-10-05. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** in progress. Step 1 done (the transport stream and the AC-3 decoder, matching FFmpeg);
step 2 waits for ITU-T H.262 (Andy is fetching the PDF: the ITU's site refuses scripted downloads).
Step 4's stream is on unit A and plays in VLC; step 3's sound plays in the Viewer. The picture
waits for step 2.

## Why

083 showed the receiver's transport streams play in VLC and ffmpeg. Browsers cannot decode them:
WebCodecs and Media Source Extensions have no MPEG-2 video. Andy, 2026-10-05: the Viewer's player
is our own decoders in Rust compiled to WebAssembly, "and option to play via vlc or other stream
supporting app as well". The project is open source; FFmpeg's LGPL decoders were an option, and he
chose in-house Rust, MIT like the rest of the scanner. This is also 079 item 11's third piece of
evidence for bake D.

## Design

- **The decoders:** a crate of its own, `scanner/player` (`atsc-player`, MIT, no dependencies),
  written from the standards: ATSC A/52 (AC-3), ITU-T H.262 (MPEG-2 video, Main Profile: 480i,
  720p, 1080i), ITU-T H.222.0 (the transport stream and PES). FFmpeg (8.0.1 on the PC) is only
  the test oracle, as SDRTrunk is for P25: our frames and samples are compared with its decode
  of the same files.
- **WebAssembly without a toolchain in the image build:** the crate builds to `wasm32` with plain
  exported functions (no wasm-bindgen); the built module is committed beside the UI's scripts
  and embedded like them, with a hash of the crate's sources that a scanner test checks, so a
  source change without a rebuild fails the tests. The SD image's build stays as it is.
- **The Viewer:** a Worker runs the decoders; frames go to WebGL (YUV to RGB, BT.709 or BT.601,
  deinterlaced), sound to an AudioWorklet ring buffer (never `<audio>` on a stream), the two kept
  together by their timestamps.
- **The stream:** a transport-stream hub in the scanner. Its source is a recorded `.ts` on the
  SD card played in real time (paced by its PCR) until bake D, then the PL's packets. It serves
  the whole multiplex or one programme over HTTP, to VLC and other players (the Viewer gives
  the address and a playlist) and to the Viewer's own player, which fetches the same route.

## Steps

1. The crate: the transport stream and PES (timestamps), the AC-3 decoder; tests against FFmpeg.
2. The MPEG-2 video decoder; tests against FFmpeg.
3. The WebAssembly build, the Viewer's Worker, WebGL and AudioWorklet; playback from a URL.
4. The scanner's transport-stream hub and its HTTP route; the VLC link.
5. Unit A: a recorded `.ts` through both paths.

Test input: `runs/atsc/captures_20261004/ts/*.ts` (083: ten 8 s multiplexes from unit A).

## Step 1: the transport stream and AC-3

### The demultiplexer (`scanner/player/src/ts.rs`)

- Bytes in any pieces (as a socket delivers them); packets found by their sync bytes, two a
  packet apart when it has to find its step again.
- The PAT and PMTs: long-form sections, current ones only, CRC-32 checked. The PES packets of
  one programme's elementary streams (or every programme's), with their PTS and DTS (90 kHz,
  33 bits). A PES packet of declared length (audio) is out as soon as it is whole; one of length
  0 (video) when the next starts.
- A packet with the transport error indicator set (083 sets it where Reed-Solomon failed) is
  dropped whole, since its PID cannot be trusted; the gap it leaves in the continuity counter marks
  its PES packet damaged. Duplicate packets are dropped.
- A stream is followed from its programme's PMT on.
- `packet_pcr` reads a packet's programme clock reference, for the hub's pacing (step 4).

### The AC-3 decoder (`scanner/player/src/ac3/`)

From ATSC A/52:2012, its decoding process in order: the syncinfo and BSI with both CRCs; each
block's side information (block switching, dither, dynamic range, coupling, rematrixing, exponent
strategies) with the reuse rules between blocks; the exponents; the parametric bit allocation
(the tables read from the standard's text by a script, the pseudo-code followed as printed apart
from a stray semicolon in `calc_lowcomp`'s listing); the grouped and plain mantissas, with dither
where no bits were sent; decoupling, rematrixing, dynamic range; the inverse transform through
128- and 64-point complex FFTs, the Kaiser-Bessel derived window computed exactly (within 6e-6 of
Table 7.33's five decimals), overlap and add. A two-channel LoRo downmix for the player.

### Tests against FFmpeg

`scanner/player/tests/`, with `ATSC_PLAYER_DATA=runs/atsc/captures_20261004` (the reference
files and how they were made: that folder's README, `player_ref/`):

- **The streams** (`ts_vs_ffmpeg.rs`): each programme's AC-3 stream from our demultiplexer is
  FFmpeg's copy of it byte for byte from the PMT on (FFmpeg also keeps up to 15,360 bytes from
  before it: the captures' first PAT is 0.2 s in), and every PES packet's PTS (282) is FFmpeg's
  for the same syncframe. The stations' AC-3 timestamps step by 2875 to 2886 ticks a frame rather
  than 2880 (RF 30 directional), and FFmpeg reads the same values.
- **The sound** (`ac3_vs_ffmpeg.rs`): dither makes no two decoders' output equal, so each long
  block's coefficients are taken back out of both decoders' output by the forward transform
  (A/52 8.2.3.2) and compared in the bins the encoder sent bits for. Every frame decodes, and the
  coefficients agree to:

| Stream (capture, programme) | Audio | Frames | Agreement, per channel (dB) |
|-----------------------------|-------|--------|------------------------------|
| RF 14 directional, 3 | 3/2 + LFE, 384 kb/s | 244 | 77.3 to 97.6 |
| RF 14 directional, 4 | 2/0, 192 kb/s | 244 | 91.1, 93.5 |
| RF 19 directional, 3 | 3/2 + LFE, 384 kb/s | 245 | 66.0 to 95.0 |
| RF 20 omni, 3 | 3/2 + LFE, 384 kb/s | 242 | 79.0 to 94.0 |
| RF 21 omni, 3 | 2/0, 192 kb/s | 242 | 93.1, 95.6 |
| RF 30 directional, 3 | 2/0, 192 kb/s | 243 | 91.6, 92.9 |

The lowest (66 dB) are RF 19's quiet surrounds, at FFmpeg's float output's own precision.

**FFmpeg's step.** The first comparison failed on two streams (RF 19's right channel at 27.7 dB,
RF 20's left surround at 44.9 dB), each from a single block. The cause is FFmpeg's: at the first
block in a stream whose channels mix long and short transforms, it rebuilds the previous block's
overlap as though the output had been downmixed, so in 3/2 the right channel takes the centre's
overlap and the centre and surrounds are zeroed. Its output steps there in four channels at the
block's first sample (RF 19, frame 219, block 5: the right channel from -1.3e-3 to -25.2e-3, the
centre's value), in both its float and fixed-point decoders; ours is continuous in every
channel. It happens once per stream (RF 14 programme 4 block 60, RF 19 block 1319, RF 20 block
1018), and the test leaves out the two transforms spanning that block.

## Step 3, the sound: the module and the Viewer

- **The player** (`scanner/player/src/player.rs`): a programme from its transport stream, the
  demultiplexer then the AC-3 decoder, with the AC-3 framed across PES packets. A PES packet's PTS
  goes to the first syncframe starting in it and the frames after it take the last time plus
  1536 samples; a frame that fails its CRC comes out as silence of its length, so the sound keeps
  its time. `player_vs_ffmpeg.rs`: on the six programmes, fed in uneven pieces, every syncframe
  plays at the time FFmpeg gives it (230 to 240 each), none lost.
- **The module** (`src/wasm.rs`): plain exports over a player the page holds by address, with an
  input buffer and an output buffer in the module's memory; no imports. 128 KB.
  `tools/build_player.sh` builds it into `scanner/src/ui/static/player/` with the hash of the
  crate's sources (`atsc_player.source`); the scanner's test fails when the crate changed since.
  The UI's assets became bytes so the module is embedded like the scripts, served as
  `application/wasm`.
- **The page** (`js/tv/`): the module compiled once and handed to a Worker that fetches the
  stream (`/api/v1/atsc/stream/{rf}?program=`), feeds the player and sends each syncframe's sound
  with its time to an AudioWorklet ring (`StereoRing`: 48 kHz in, a slow PLL on its fill, target
  0.5 s, the playing sample's time kept for the picture to follow), or to the same ring under a
  ScriptProcessorNode on plain http, where browsers hide AudioWorklet. The Viewer offers it for a
  channel whose RF channel has a recording holding its programme (`/api/v1/atsc/recordings`):
  Play sound and Stop, the player's state (format, buffer, frames lost), the stream's address
  and a playlist file for VLC.
- **Checks:** the page's Worker source run in a Node worker thread against unit A's stream for
  20 s: 650 syncframes (20.8 s of sound), the time stepping back only at the two loop seams, no
  underruns, two frames lost (the frame each seam cuts). It caught one bug before the page ran:
  the Worker read the description's address before asking for the description. On unit A in
  ATSC mode after a TV scan of RF 14-30 (build `2026-10-05-tv-sound1`), headless Chrome on the
  Viewer: every channel of RF 14 and 20 playable; Play gives "Playing" within 2 s over http
  (ScriptProcessor) and https (AudioWorklet), the buffer at 0.4-0.54 s, no exceptions. A went
  back to scanner mode on Clay County after.

## Step 4: the stream

`scanner/src/services/atsc/stream.rs`, two routes:

- `GET /api/v1/atsc/recordings`: the TV recordings on the SD card (`tv/rf<N>.ts`), each one's RF
  channel, size and the programmes its PAT lists.
- `GET /api/v1/atsc/stream/{rf}` (`video/mp2t`): the RF channel's transport stream as it plays,
  for VLC and other players now and the Viewer with step 3. The recording is read at the pace of
  its clock references (each piece between two of them sent when the first is due) and from the
  start again at its end, the first clock reference after the seam marked discontinuous. With
  `program`, only that programme: a PAT naming it alone (the original's transport stream id and
  version), its PMT, streams and clock, and PSIP's base PID for the station's names. Each player
  gets its own reading; 404 for a channel with no recording or a programme not in it.

Its source is a recording until the radio core decodes 8-VSB (bake D); the route and what it
serves stay the same then.

**On unit A** (build `2026-10-05-tv-stream1`, scanner mode, Clay County live throughout): 083's
clean multiplexes copied to `/mnt/sd/tv/` (RF 14, 19 and 30 directional, RF 20 and 21 omni).

- The whole of RF 14 came at 19.3 Mbit/s and its programme 3 at 12.17 Mbit/s over 12 s, exactly
  that programme's share of the recording's packets (62.8 %), the first bytes 57 ms after the
  request. FFmpeg finds programme 3 alone (720p MPEG-2, AC-3 5.1 and stereo) and decodes it with
  errors only at the loop's seam.
- VLC 3 plays `http://192.168.120.50:8080/api/v1/atsc/stream/19?program=3` (its own MPEG-2 and
  AC-3 decoders). It reports continuity gaps on the video PID: they are the recording's own, the
  packets Reed-Solomon could not correct in RF 19's capture (121 flagged in 8.6 s), passed on
  flagged as they were. VLC also lists programmes 4-6 from PSIP's channel table, empty.
