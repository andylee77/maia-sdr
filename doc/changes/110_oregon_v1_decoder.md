# 110 — The first decoder: Oregon Scientific version 1, and Devices

**Date:** 2026-11-12. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. In unit A's SD image, `2026-11-12-decode1`, with 108 and 109.

## Why

109 left Data mode hearing Andy's Oregon Scientific sensor and reading nothing: its temperature
came from a check of a capture on the PC. Asked "Decode your sensor on the unit and show its
temperature on the page next?", he answered "Yes, decode it". He had asked for temperatures in
°F, with the unit in Settings (109); this gives that setting its first reading to show.

It is the first of the study's step 5 (`scanner/doc/DATA_MODE.md` §9): the pulse front end and
one hand-written decoder, for the one device known to be on the air.

## Changes

### `protocol::ism`: a burst's pulses, and the decoder

- **`pulses`:** a burst's IQ (the channel's 250 kSPS, with 20 ms either side) brought to the
  burst's own centre, filtered to 15 kHz either side, decimated to 50 kSPS and read as an
  envelope that is on or off: on is 35 % of the way from the envelope's median to its peak, the
  slice the look of 109 read the first captures with. Every decoder of an on-off keyed device
  works from it.
- **`oregon_v1`:** written from the protocol's facts, not from another program's code.
  Manchester at 342 bits a second, a bit whose first half is on being a one; a sync pulse of
  about 5.7 ms between two long gaps; 32 bits after it, each byte's least significant bit
  first: the sensor's id and channel, the temperature as three decimal digits, a flag for
  under zero and one for a low battery, and a fourth byte that is the sum of the first three
  with the carry added back. The decoder finds each sync pulse, reads the 32 cells from where
  the data should begin (tried 0.8 ms before to 1.6 ms after, the best-formed reading
  winning), and takes a message whose every cell is Manchester coded, whose sum is right and
  whose temperature is decimal. A burst holds the message twice: the reading says how many
  times it was read.
- **`Reading`:** what a device said in one burst, as every decoder gives it: model, kind, id,
  channel, temperature in °C, battery, repeats. A device's key is `<model>.<id>.<channel>`.

### `services::data`: readings, devices and their logs

- **Each burst that ends is handed on with its IQ** (always, not only when a capture is asked
  for) to a thread of its own (`data-bursts`, which was the captures' writer): it has the
  decoders read the burst, then writes its capture when one was asked for. The listening thread
  does neither.
- **A burst a decoder reads** carries its reading; its emitter is named as the device; and the
  **device** (by what it is, wherever it is heard) has its messages counted, how often it sends
  (the median of its last 16 intervals), what it last said and a **log** of the last 200 things
  it said. All in memory, as the emitters are.
- **A capture's description** carries what its burst said (`fishball:burst.reading`), and the
  list shows it. A capture kept before this has none.

### The API

- `GET /api/v1/data`: `devices`, each burst's `reading`, each emitter's `device`.
- `GET /api/v1/data/devices/{key}`: a device with its log.
- `status.data.devices`, and `reading` in `GET /api/v1/data/captures`.

### The pages

- **Devices,** laid out as the study draws it (§7): the devices in a table (Kind, Device, ID,
  MHz, Last, Every, the level over the floor, Latest), and under it the picked device's data
  log: its temperature as a line with the newest reading and the range, "Keep next" (the
  drawing's "Capture next": its next burst's IQ, asked for within 3 kHz of it), and the entries
  (time, MHz, levels, reading, capture). Not built, and left out: the filters, Name and Ignore.
- **Scan:** an emitter a decoder reads is named ("Oregon Scientific v1 · id 3 · channel 1"), its
  signature says "decoded" and "Open" goes to Devices, as drawn; a burst's dash says what it
  said when pointed at. An emitter's own "Keep" now asks within 3 kHz of it: at the 10 kHz it
  asked before, the sensor's also took the device 8.4 kHz under it (seen in 109's gate).
- **Captures:** a Decoded column.
- **Temperatures follow the radio's setting** (`units.temperature`, 109): the pages read it at
  their start and when the radio's settings change, and show °F or °C; the API's fields stay
  in °C. The Units card says Data shows a sensor's reading in it.

### The tool

`tools/data_captures.py`: `status` lists the devices; `keep --oregon-v1` holds what the scanner
read of each kept burst against what `look` reads of its capture on the PC (the id, the
channel, the temperature, the battery and how many times), and fails on a difference.

## Not in this change

- **rtl_433 as the reference.** The study's gate for a decoder is that it matches rtl_433 on
  the corpus; rtl_433 is not on this PC. What stands in: the message's own checksum, the two
  copies in each burst agreeing, the PC tool's reading of the same captures (written apart
  from the Rust, from the same facts), and temperatures that fall through an evening as an
  outdoor sensor's would. The id's and the channel's places in the first byte are as the
  protocol's description has them and nothing here checks them: this sensor reads as id 3,
  channel 1.
- The table-driven decoder and every other device: the three other devices at 433.92 MHz are
  not read.
- The data log on the card and its tables in the history's database; names; the filters and
  kinds beyond "weather"; an export of a device's log.
- A burst taken apart by frequency: when the sensor and the device 8.4 kHz under it send at
  once the slice still reads the sensor, which is 12 dB the stronger, and would not read the
  weaker.

## Checks

See the status log for the counts.

- **Host tests:** the pulses (a keyed burst's pulses and gaps at their lengths; a burst read off
  its centre); the decoder on made messages (the sensor's own bytes; twice in a burst; under
  zero, another channel, a low battery, weak; a wrong checksum, a temperature not decimal, a
  message cut short, another device's keying, noise, a sync with nothing after it); the
  service (a read burst's device, its interval and log, its capture's reading; a burst nothing
  reads).
- **The captures** (`DATA_CAPTURE_DIR=<repo>/runs/data/look_20261112 cargo test --release
  ism::`): every burst of the sensor in the first three captures reads as the look read it,
  twice, and no other device's burst reads as anything; the unit's own captures of it (three,
  copied there) read as the PC tool read them.
- **`runs/ui_check/data_listen.mjs`:** the device on Scan and on Devices, and the unit of
  temperature changed and put back (on the PC daemon only).
- **`tools/data_captures.py keep --take-radio --oregon-v1`** on unit A: the gate, the scanner's
  own reading of each kept burst the same as the PC's reading of its capture.

## Status log

- 2026-11-12: built. Host tests 622 pass (8 new: the pulses 3, the decoder 2, a reading's key 1,
  the unit's captures 1, the service 1); the DMR reference's tests pass (102); the ARM check and
  the ARM release build are clean. BUILD_TAG `2026-11-12-decode1`.
- 2026-11-12: **the captures through the decoder** (release, the 24 tests of `ism::` in 1.7 s):
  the three captures of the window give six bursts each as before, and the sensor's five (at
  7.992 and 38.003 s, 6.936 and 36.946 s, and 17.880 s) each read as id 3, channel 1, 15.9 °C,
  twice; none of the other thirteen reads as anything. Of the unit's own captures from 109's
  gate, the three of the sensor read 14.5 °C, as the PC tool read them, and the one of the
  device at 433.871 MHz reads as nothing.
- 2026-11-12: on the PC daemon, the first capture replayed as its radio: `data_listen.mjs` 14
  steps at two sizes (two more than 109's: the sensor named on Scan and listed on Devices with
  its log, and the unit of temperature changed in Settings, shown on the page and put back).
  `data_mode.mjs` 13, `tool_pages.mjs` 31 at two sizes, `files_page.mjs` 15 and over HTTPS,
  `now_rail.mjs` 29 and over HTTPS, `playlist_page.mjs` 16, `aliases_page.mjs` 34,
  `access_page.mjs` 20 over HTTPS, `modules_check.mjs` 51 modules.
- 2026-11-12: **`tv_watch.mjs` 10 of 12:** its two full-screen steps fail. The headless Chrome on
  this PC (157.0.8103.78) refuses full screen: asked on a blank page with nothing of ours, with
  a user gesture, `requestFullscreen` answers "not granted". The check passed all 12 with 109
  and the Watch page is unchanged since; why Chrome grants none now is not known. Full screen
  was not tried in a browser with a window.
- 2026-11-12: `2026-11-12-decode1` on unit A (binary only, md5 `864c08cf…`, in RAM over the SD
  image `2026-11-11-ssh1`: a reboot of A returns to that image, where Data is a shell; A was
  restarted for it at about 09:18 UTC):
  - **the gate, the unit's own reading of the sensor:**
    `tools/data_captures.py --host 192.168.120.50 keep --count 2 --freq 433880000 --span 6000
    --wait 110 --take-radio --oregon-v1` exits 0 (`runs/data/gate_110.json`): two captures
    30.010 s apart (`20261112T092010Z_0005`, `20261112T092040Z_0009`), and the scanner's reading
    of each burst the same as the PC's reading of its capture: id 3, channel 1, 13.0 °C
    (55.4 °F), the battery good, twice (the bytes `03 30 01 34`). No gap in the run and no burst
    dropped; the radio given back to the scanner;
  - **the cost:** the listening thread at 73 % of one core, as in 109; the thread that reads
    the bursts too small to see beside it; the process 85 %;
  - `data_listen.mjs` 13 steps and `data_mode.mjs` 13 with A serving the page: Devices lists
    "Weather, Oregon Scientific v1, 3, channel 1, 433.879, 30 s, 31.4 dB, 55.4 °F" with its data
    log, in °F as A's setting has it; reads only, over HTTP: `scanner_pages.mjs` 40,
    `tool_pages.mjs` 27, `now_rail.mjs` 15, `files_page.mjs` 9, `aliases_page.mjs` 9,
    `playlist_page.mjs --preview-only` 13, and over HTTPS: `tool_pages.mjs` 27, `now_rail.mjs`
    15; `API_FIELDS.md` regenerated from A;
  - **left listening four minutes after the checks:** eight messages of the sensor's, every
    30.009 s, each read (one in a burst of 589 ms that also held the device 8.4 kHz under it),
    13.0 to 13.1 °C. A is back in scanner mode, live on Clay County at 100 %.
- 2026-11-12: **the SD image `2026-11-12-decode1`** (fishball-p25 3af4679, tezuka_fw 2bcddb0,
  the radio core 2.1.0) on unit A, its boot files also in `/mnt/sd/boot_new_decode1` (ssh1's in
  `/mnt/sd/boot_new_ssh1`). Asked "Build an SD image with Data mode (108 to 110) for unit A
  next?", Andy answered "Yes, build it". It is the first image with 108 to 110; the firmware is
  ssh1's.
  - **Built** in 4 min 33 s, the image's Rust 1.86 compiling the scanner in 3 min 13 s: the
    code of 108 to 110, which only this PC's Rust had compiled, builds with the image's too.
  - **Checked before installing** (the ramdisk read on the PC): the scanner carries the tag and
    the marks of 108 to 110 (the decoder's model, the two threads' names, the device's route,
    the Devices page, the captures' folder, packet data's route), none of which ssh1's scanner
    has. SSH is as ssh1 had it: the server told to take no password, the key this PC's, in a
    file of root's alone. Against ssh1's ramdisk nothing differs but the scanner and what
    differs at every build: the root password's salted hash (the same scheme and digest; the
    salt's length varies too, 11, 13, 8 and 10 characters in the last four images) and 16 bytes
    of the USB drive's image. `BOOT.bin` differs in 6 bytes, U-Boot's build time, so the
    bitstream is the same; the device tree and `uEnv.txt` are ssh1's; the kernel was built
    again. The card's copies matched the build's sha256 before the reboot.
  - **After the reboot** (asked at 10:47 UTC): live on Clay County 106 s after it was asked,
    control decode 100 % at 40 messages a second. The binary on A is the image's (its sha256
    the one read from the ramdisk). Open as before, libiio off and its choice kept off, the
    temperature's unit still °F, the fourteen captures still on the card. HTTPS verified by the
    CA file saved before the reboot and refused without it. SSH offers `publickey` alone; a
    login by key works.
  - **The gate from the image:** the same command exits 0 (`runs/data/gate_110_image.json`):
    two captures 30 s apart, and the scanner's reading of each the same as the PC's reading of
    its file: id 3, channel 1, 13.3 °C (55.9 °F), twice (the bytes `03 33 01 37`). The first of
    them is a burst of 570 ms that also holds the device 8.4 kHz under the sensor. No gap, no
    burst dropped, the radio given back.
  - **The pages, A serving them:** `data_listen.mjs` 13 steps and `data_mode.mjs` 13; reads
    only, over HTTP: `scanner_pages.mjs` 40, `tool_pages.mjs` 27, `files_page.mjs` 9,
    `aliases_page.mjs` 9, `playlist_page.mjs --preview-only` 13; over HTTPS: `tool_pages.mjs`
    27, `files_page.mjs` 9. `scanner/doc/API_FIELDS.md` regenerated from A.
  - **`now_rail.mjs` 11 of 15, over HTTP and HTTPS:** its four steps that play a recording
    found none to play. Now lists the calls since the scanner's start, and in the 40 minutes
    after this reboot (to 11:27 UTC, before half past six in the morning there) no call was
    recorded: one was followed, TG 300 at 10:57:49, and it carried no voice (the lane decoded
    the site's four call terminations two seconds after the grant, so the lane hears; the 40
    calls of that talkgroup before the reboot all had voice and a recording). Lane 2 decodes
    the data channel's packets meanwhile. So no voice call has been recorded from this image
    yet: none was sent on a talkgroup it follows.
- **Seen and not changed:** a capture's description writes its 32-bit numbers with a 64-bit
  number's digits (`"temperature_c": 13.300000190734863` for 13.3 °C; the levels likewise since
  109): it is built as a JSON value, which holds every number as 64 bits. The API's list and
  the pages give 13.3, and the number reads back as it was.
- **Seen and not changed:** a fifth emitter at 433.970 MHz (three bursts of about 370 ms, 13 to
  14 dB over the floor) that the first look did not have: not looked at. Eight captures on A's
  card from 04:14 to 04:17 UTC that no check of mine asked for (asked from the page by another
  client): left there.
- **Not checked:** rtl_433 on the same captures; the id's and the channel's places in the
  message against another reference; hours of listening; on the air, another sensor or
  channel, a temperature under zero or a low battery (made ones are in the host tests); the
  pages in a browser other than headless Chrome; an image with it.
