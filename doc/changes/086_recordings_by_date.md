# 086 — Recordings in dated folders, kept by age, alert calls locked

**Date:** 2026-10-06. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** done; unit A runs the SD image `2026-10-06-rec1` with it.

## Why

Unit A stopped saving recordings to its card at 2026-10-05 21:21 UTC. Every write since has failed
with "No space left on device" and gone to RAM, where only the newest 40 are kept until a reboot:
2,137 failed on the image `2026-10-05-bake-d2` alone. The card had 56.6 GB free. The recordings
folder held 8,640 files in one directory, and FAT32 allows a directory 65,536 32-byte entries
(2 MB): a file with a 79-character name takes 7-8 (its long name in 13-character pieces, and its
short name). The folder was at exactly 2,097,152 bytes. A's retention allowed 20,000 recordings
and 2,048 MB (the default is 2,000), and the 8,640 came to 546 MB, so nothing was ever deleted.
The card's status meanwhile said `ok`: a failed write set it to `full`, and the next probe, finding
plenty of space, set it back.

## Andy's decisions (2026-10-06)

- **Dated folders,** named by the UTC date of the call's start, and a cap on the files in one.
- **Retention by age** in place of a maximum count: recordings older than the days kept are
  deleted, the oldest first.
- **Alert calls locked** to keep them longer, and a Lock button for any recording.
- **Defaults:** 30 days, locked ones 365, at most 8 GB on the card, 4,000 files a folder.

## Measured on A's card

- **Listing names only is linear and quick:** cold (caches dropped), 0.10 s for a folder of 2,500
  files and 0.31 s for the 8,640. The boot listing reads names only, so thirty days of Clay County
  (about 2,500 recordings a day, 75,000 files) lists in about 3 s.
- **Each lookup by name scans its folder:** a stat, create, rename or delete. BusyBox `ls`, which
  stats every file, took 22 s cold on the 2,500 and 3 min 52 s on the 8,640; deleting the 2,500
  took 12 s. Smaller folders make each write and delete cheaper.

## The layout

```
p25_recordings/2026-10-06/rec_<start ms>_<call>_tg<tg>[_from<src>][.<site>].wav
p25_recordings/2026-10-06_2/…           when the day's first folder holds the cap
p25_recordings/locked/2026-10-06/…      locked recordings, by the same rule
```

- File names do not change; the history keeps recordings by name, so it needs no change either.
- A recording goes to the first folder of its day (UTC, from the call's start) holding fewer
  than the cap, counted from the list in memory.
- The boot listing walks the dated folders and `locked/`'s; a `.wav` left at the top is counted
  in the listing's note, not listed. A folder emptied by retention is removed.
- A's existing files are moved into the new layout once, by a script over SSH (the scanner reads
  only the new layout); the newest recordings in RAM are copied onto the card first.

## Retention

- **By age:** on the card, an unlocked recording older than `sd_keep_days` and a locked one older
  than `locked_keep_days` are deleted. Age needs a valid clock (the board starts in 1970): with the
  clock unset, or a recording stamped before 2020, nothing is deleted by age.
- **By size:** past `sd_max_mb` on the card, the oldest unlocked recordings go first; locked ones
  count towards it but are never deleted for size. The newest recording on the card always stays.
- **RAM** keeps its count (`ram_max_count`).
- **When:** after each recording, when the settings change, after the boot listing, and hourly.

## Locking

- A call whose recording has alert tones is saved locked.
- `PUT /api/v1/recordings/{id}/lock` `{"locked": true|false}` moves a card recording between the
  trees (through the card writer's queue, behind any write of it) and returns it. A recording in
  RAM cannot be locked.
- The list and `/api/v1/recordings` carry `locked`; the Now page shows it with a Lock/Unlock
  button.

## The card's status

A write that fails while the card is mounted with space sets `failing` (with the error), which a
probe no longer clears; the next successful write sets `ok`. New recordings still try the card
while it is `failing`, so it recovers by itself.

## Settings

`radio.json`'s `recording`: `sd_keep_days` (30, 1-3,650), `locked_keep_days` (365, from
`sd_keep_days` to 3,650), `sd_max_mb` (8,192), `folder_max_count` (4,000, 100-6,000); `sd_max_count`
goes. Settings shows them; a lowered retention deletes at once, as before.

## Steps and checks

1. This plan.
2. The scanner: the layout, retention, locking, the status, settings, the API and the UI, with
   tests; host tests, the ARM check, the DMR reference, the UI checks.
3. On unit A: the RAM recordings saved, the files moved, the test build deployed; recordings land
   in today's folder, the listing finds them all, playback, a lock and an unlock, the settings at
   the defaults. Retention by age is checked by the host tests: nothing on A is 30 days old yet
   (the oldest, from 2026-10-02, goes on 2026-11-01).
4. An SD image with it.

## On unit A

- **The move** (`runs/086/plan_move.py` wrote the script from the scanner's own list; the scanner
  stopped, the card mounted): 8,640 files into `2026-10-02` (1,593), `-03` (2,259), `-04` (1,208)
  and `-05` (3,362), and the 218 with alert tones into `locked/2026-10-03`, `-04`, `-05`; built in
  `recordings_new/` and put in place of the old folder, so the 2 MB directory went. It took 5 min
  30 s, all of it the kernel scanning the full folder for each name. The 41 recordings only in
  RAM (copied out through the API just before) went into `2026-10-06` and `locked/2026-10-06`.
- **The scanner after:** the boot listing found 8,681 recordings with nothing skipped; new
  recordings land in `2026-10-06/`, written in 12 ms (200-400 ms in the full folder), none failed;
  the card `ok`, 219 locked. A lock moved a recording to `locked/2026-10-06` and an unlock back,
  playable both ways. A's settings had kept `sd_max_mb` 2,048: now the defaults, 8,192.
- **The UI:** Now draws a lock button on each card recording (64 of 103 calls), Settings the new
  rows and "SD card (ok, 56.58 GB free): 8,704 recordings, 422.0 MB, 219 locked"; no exception
  in headless Chrome on A or on the PC.
- **`fsck.fat`** at the start reports a difference between the boot sector and its backup (offset
  65, the dirty flag), as it did at the boot before: unrelated, left alone.
- **`API_FIELDS.md`** regenerated from A. Its meanings lacked 082's spectrum and capture readback
  and 082's and 085's routes (the burst check, live TV); they are written now.

## Status log

- **2026-10-06.** Plan. Andy: dated folders by UTC date, retention by age (30 days, locked 365),
  8 GB, 4,000 a folder; alert calls locked, and a Lock button.
- **2026-10-06.** The scanner's side, with tests (host 515, the ARM check, the DMR reference, the
  UI checks); on unit A the files moved and the test build `2026-10-06-rec1` running (above).
- **2026-10-06.** The SD image `2026-10-06-rec1` (fishball-p25 7473ddb, tezuka_fw 2827b89, the
  radio core 2.1.0's fifth build) on unit A, its boot files also in `/mnt/sd/boot_new_rec1`
  (bake-d2's in `/mnt/sd/boot_new_baked2`). After the reboot the listing found 8,792 recordings
  and new ones wrote in 12 ms; A on Clay County.
