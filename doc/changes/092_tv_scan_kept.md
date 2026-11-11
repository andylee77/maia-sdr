# 092 — The TV scan is kept across a restart

**Date:** 2026-11-10. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** done; in unit A's SD image since `2026-11-10-tv2` (the first with 087 to 092). A
has a kept scan of 8 stations on its flash.

## Why

A TV scan's results lived in memory. After a restart of the scanner Watch (091) listed no
channel until a scan ran again, which needs the radio and about a minute. Andy asked for the
list to be kept, so Watch is never empty.

## Changes

- **A scan that finishes is kept** with the unit's state, in `state/tv_scan.json` on the flash
  (`services::atsc::kept`): its id, its times, the channels asked for, the gain, and every channel
  read with its station, virtual channels and programmes. Unit A's scan of 33 channels with 7
  stations is 39 KB.
- **At start it is the last scan again** (`Atsc::restore`): Watch lists its channels, Channels
  shows its table, and live TV starts its carrier loop from the pilot offsets it measured.
- **A scan cut short or failed is not kept.** The last one that finished stays on the flash, and
  comes back at the next restart.
- **The windows' spectra are not kept** (18 windows of 4,096 bins). After a restart a channel's
  row on Channels opens with its station and the Watch buttons, and says its spectrum comes
  with the next scan; `GET /api/v1/atsc/scan/channel/{n}` answers 404 until then.
- **A factory reset forgets it.** The configuration export does not carry it, like the rest of
  what the radio learned on the air.
- The kept types read back from JSON: the scan's channel, station, virtual channel, programme
  and stream types gained `Deserialize`. A stream's kind and a picture's aspect are names out
  of fixed lists, read back to those; a channel's band comes from the plan again.

## Checks

- **Host tests:** 519 pass, three of them new:
  - a kept scan reads back as it finished (a station with its programmes and streams, a channel
    not decoded, a channel with no station), and forgetting it leaves none;
  - what does not read is no scan: one that never finished, a channel the plan lacks is left
    out, a file with a kind no stream has is moved aside;
  - a scan run on the test radio is the last scan in a new service started on the same file,
    without its spectra, and a cancelled scan after it leaves the kept one as it was.
- **On the PC daemon** (`2026-11-10-tv2`), with unit A's scan written to its state folder: after
  start `GET /api/v1/atsc/scan` gives it as done (7 stations, 37 channels), a channel's spectrum
  is a 404, and `tv_watch.mjs --kept` passes 13 steps: Channels shows the table, RF 19's row
  opens with its four Watch buttons and the note about its spectrum and no error, and Watch
  lists and plays as in 091.

## Status log

- 2026-11-10: committed (cbac1c9). Host tests 519 pass; the DMR reference keeps 24,984 of 24,996
  lines; the ARM release build is clean.
- 2026-11-10: `2026-11-10-tv2` went onto unit A (binary only), on the radio antenna:
  - live on Clay within 34 s, control decode 100 %; no scan yet, as nothing was kept;
  - a TV scan through the API: 33 channels read in 61 s, 8 stations named with 53 channels;
    `state/tv_scan.json` is 85 KB, and the flash has 652 KB of its 896 KB free after it;
  - the scanner restarted: live within 33 s, and the scan is there again as done (8 stations,
    53 channels); RF 19's spectrum is a 404;
  - the pages in headless Chrome, read only: Watch lists 53 channels; on Channels RF 19's row
    opens with four Watch buttons and "its spectrum is not kept across a restart of the
    scanner", with no error;
  - live TV on RF 19 through the API started from the kept scan's pilot offset (-6 Hz) and was
    playing 8 s later at an MER of 23.1 dB; stopped, and the scanner back live at 100 %.
- `scanner/doc/API_FIELDS.md` is regenerated from unit A.
- 2026-11-10: the SD image `2026-11-10-tv2` (fishball-p25 782e3da, tezuka_fw 2827b89, the radio
  core 2.1.0's fifth build) on unit A, its boot files also in `/mnt/sd/boot_new_tv2` (rec1's in
  `/mnt/sd/boot_new_rec1`). It is the first image with 087 to 092.
  - **Checked before installing:** the ramdisk's scanner carries the tag and the changes' code;
    `BOOT.bin` differs from rec1's in 11 bytes, U-Boot's build date, so the bitstream is the
    same; the card's copies matched the build's sha256 before the reboot.
  - **After the reboot:** the scanner started 55 s into the boot and was live on Clay County
    within 46 s, control decode 100 % at 40 messages a second; 101,552 recordings listed and new
    ones written to the card in 10 to 12 ms; `packet_data.follow` on; the kept scan there as
    done (8 stations, 53 channels) with no scan run.
  - **The pages, A serving them** (headless Chrome): `now_rail.mjs`, read only, passes its 10
    steps; `tv_watch.mjs --unit --kept` passes 17: Watch lists the 53 channels while the
    scanner has the radio, RF 19's row on Channels opens without its spectrum, 47.1 WJAXHD plays
    live with no scan run since the boot (MER 20.3 dB on the radio antenna), the large view,
    47.2, full screen, 30.1 on RF 14, Stop, and Return to Scanner brought Clay back.
