# 101 — Files: what is on the card, to take away

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. On unit A as a test build in RAM (`2026-11-11-files1`) over the SD
image `2026-11-10-pages2`, which does not have it.

## Why

The roadmap's step 5. A recording left the card one at a time, the history as CSV, and a TV
recording only as a stream played at the pace of its clock. Andy asked for "SD downloads"; the
roadmap's Q6 recommended a Files page with one ZIP archive of a period, a talkgroup, a radio or
the locked ones, and a command-line tool doing the same through the API. Asked "Build the Files
page next (step 5), with recordings downloaded as a ZIP archive?", he answered "Yes, Files page"
(2026-11-11).

The roadmap's condition: a download must not cost the scanner a call. It is one at a time, and at
a pace the card and the A9 can give while two lanes decode, measured here.

## Changes

### The routes

| Route | What |
|-------|------|
| `GET /api/v1/files` | What is on the card: its state and space; the recordings on it, locked and in RAM, and by span of time (`span`: 15, 30, 60 or 1440 minutes); the history's database; the TV recordings; the download running |
| `GET /api/v1/files/recordings` | How many recordings a selection takes, and their size |
| `GET /api/v1/files/recordings.zip` | The selection as one ZIP archive |
| `GET /api/v1/files/history.sqlite` | The history's database, as one consistent file |
| `GET /api/v1/files/tv/rf<N>.ts` | A TV recording's file |
| `/ws/live`, `files` | The download running, pushed while a page asks for it |

- **A selection** is any of `from` and `to` (unix ms of the call's start), `site`, `tg`, `unit`
  and `locked=true`; none takes every recording. `unit` is a radio that talked in the call as
  far as the list knows: the one in the file's name, and for the recordings made since the
  scanner started every one.
- **The listing is by span of time, not by day,** because the radio does not know the reader's
  zone. A first version took an offset from UTC and gave days: the check found October's days an
  hour off, since the offset was November's. The page adds the spans up into its browser's days,
  with their true midnights; a span is as long as fits the zone (an hour, or 30 or 15 minutes).

### The archive

- **A ZIP file, stored and not compressed:** a recording is 8 kHz speech that a general
  compressor gains little on, and storing costs the A9 a CRC.
- **Made as it is sent.** A recording is read when its turn comes; nothing is kept of the
  archive but its directory (about 100 bytes a recording). It has no length before it ends.
- **Each recording as it is on the card,** under its name there, in the folder of its call's
  UTC date, timed at its call's start. The names are the card's so that what reads them now
  reads an archive's too.
- **`recordings.csv` is its last file:** `file, started_utc, site, talkgroup, talkgroup_name,
  radio, radio_name, duration_s, bytes, locked, call`, with the names the aliases give.
- **A recording gone from the card** between the listing and its turn (the retention, a delete)
  is left out of the archive and of the table, and counted.
- **ZIP64** records are written when the archive has 65,535 files or more, or passes 4 GiB: the
  whole card is one such (`util/zip.rs`).
- **Its name** says its days and what narrowed it: `recordings_2026-11-10_2026-11-11_tg300.zip`.

### The history and TV

- **The history's database** is copied first, beside it on the card, as it stands at one moment
  (SQLite's backup, every page under one read: the writer goes on, and the other queries are
  not held up). The copy is sent with its length and removed. The answer starts when the copy is
  made; a copy an interrupted download left is removed at start. It is refused when the card has
  no room for it.
- **A TV recording** is sent as it is, with its length.

### One at a time, below the receivers

- **One download runs at a time** (`services/files.rs`); a second is refused with 409 and the
  name of the one running.
- **It is made on a thread of its own, below the receivers' priority** (nice 10): it takes a
  processor when they leave it. Its bytes go to the response through a short queue, so it reads
  the card only as fast as the client takes them, and a client that leaves ends it.
- **Each download is two lines in the radio's log:** what was asked for, and how it ended (files,
  megabytes, seconds; or that the client left).

### The page

**Files** is the radio's own, in the rail between Diagnostics and Settings.

- **Recordings:** how many are on the card; a selection (From and To as dates, a site, a
  talkgroup, a radio, only the locked ones) with what it takes shown before its **Download ZIP**;
  and the days as a table, each with its own archive.
- **SD card:** its state and space.
- **History:** its database's size and its download; a period's calls as CSV stay on Activity.
- **TV recordings:** each with its download.
- **The download running** is shown across the top with how far it is, and every link is off
  until it ends; pressing one says why.

A download is a link the browser saves as it arrives, so an archive larger than memory is no
matter to the page.

### The command line

`tools/scanner_files.py --host <unit> list | recordings | history | tv <rf>` does the same
through the API, saves under `runs/files/<host>/`, and checks what it saved: an archive against
its CRCs and its table (Python's `zipfile`), the history with SQLite's own check, a TV recording
by its length and sync bytes. `--health` reads the receivers before, during and after. One JSON
object; exit 0 whole and checked, 1 not, 3 refused because another download runs.

## Not in this change

- **A copy to a PC or NAS share on a schedule** (Q6's second half): not asked for.
- **Deleting from this page,** and the factory reset's delete per recording (095's review, step
  5): the page takes away, it does not remove. Left in the roadmap.
- **Captures** (data mode's): they come with data mode.
- **A download resumed** after it broke: an archive has no length, so it starts again.
- **Access:** until step 6 an archive is as open as one recording is now.

## Checks

See the status log for the counts.

- **Host tests:** the ZIP writer (an archive read back as an unzip reads it, CRC-32's check
  value, the ZIP64 end at 70,000 files, an offset past 4 GiB); a selection by each field; the
  listing by span; an archive through the service with a recording still in memory and one gone
  from the card; one download at a time; a file sent whole, and a client that leaves; the
  history's copy with the writer going on, removed after.
- **`runs/ui_check/files_page.mjs`** (new): the page against the listing; the selection
  narrowed; one archive downloaded through the page and read back; a download held open to see
  its progress, every link off and a second one refused by the page and by the radio.
- **`tools/scanner_files.py`** against the PC daemon and unit A, each download checked.
- **The other pages:** the rail has five entries (`now_rail.mjs`), and the radio's own log has a
  `files` kind (`tool_pages.mjs`).

## Status log

- 2026-11-11: built. Host tests 555 pass (13 new: the ZIP writer's six, the service's seven); the
  DMR reference's tests pass (102, holding its floor of 24,984 lines); the ARM check and the ARM
  release build are clean. BUILD_TAG `2026-11-11-files1`. `modules_check.mjs` 46 modules.
- 2026-11-11: on the PC daemon:
  - `tools/scanner_files.py`: an archive of its two recordings, the locked one alone, the
    history's copy and a TV recording, each checked; a selection that takes nothing exits 1;
  - `files_page.mjs` 15 steps at 1920x1080 and at 1280x800. Its first run failed one: the day
    picked as From and To did not take the table's day, which is how the listing came to be by
    span (see "The routes");
  - `now_rail.mjs` with the writes 29 and with unit A's reads 15, `aliases_page.mjs` 34,
    `playlist_page.mjs` 16, `scanner_pages.mjs` 40, `tool_pages.mjs` 31, `tv_watch.mjs` 12.
- 2026-11-11: `2026-11-11-files1` on unit A (binary only, in RAM over the SD image
  `2026-11-10-pages2`, which does not have it: a reboot of A returns to that image's pages). Live
  on Clay County within 34 s. Its card: 101,665 recordings (5.1 GB, 3,845 locked) over 40 days,
  the history at 211 MB, five TV recordings. Each download below was made by
  `scanner_files.py --health` and checked; the site stayed live throughout:

  | Download | Size | Time | Pace | The receivers meanwhile |
  |----------|------|------|------|-------------------------|
  | A day's recordings (2026-11-10, 2,979) | 150.7 MB | 41 s | 72 recordings/s, 3.5 MB/s | Control channel 100 %, 40.3 messages/s at the least; no counter moved; 2 recordings saved |
  | The history (339,219 calls) | 206.1 MB | 19 s, its copy included | 10 MB/s over all | The same; the daemon's workers up to 62 % of a core while it was sent |
  | A TV recording (`rf19.ts`) | 19.2 MB | 0.7 s | 25 MB/s | — |
  | The whole card (101,667 recordings) | 5.17 GB, ZIP64 | 26 min 25 s | 64 recordings/s, 3.1 MB/s | Control channel 99.5 % at the least; 46 recordings saved, none failed |

  - **The counters over the whole card's 26 minutes:** no block of control IQ dropped and none
    after a gap, no grant dropped, no lane packet lost or missed, no packet refused, no audio
    chunk missed, no recording failed. Five TSBKs failed their CRC, where the 22 minutes before
    any download had nine: the receiver's usual rate. The busiest core reached 85 %, the
    download's thread 87 % of one, the daemon's workers 48 %; the daemon grew from 100 to 124 MB
    while it held the list of 101,667.
  - **The pace is the card's directories.** The first folders (October's, of up to 8,000 files)
    went at 22 to 40 recordings a second, the later ones at 72: FAT looks a name up by scanning
    its directory (086), and the download's thread spends its time there.
  - **The archives read back:** Python's `zipfile` checked every CRC of both and found each
    table's rows to be its recordings (2,979; 101,667). Windows' own `tar` lists the 5 GB one's
    101,668 files and extracts the day's 2,979 into its two UTC folders, each file timed at its
    call's start, with the talkgroups' and radios' names in `recordings.csv`.
  - **The history's copy** passed SQLite's `quick_check`, came at the length promised, and left
    no copy on the card.
  - **The page on A,** reads only: `files_page.mjs --no-downloads` 9 steps at both sizes; opened
    while the whole card was being sent it showed "3,055 of 101,667 recordings · 130.1 MB of
    4.77 GB" with every link off. `tool_pages.mjs` 27, `now_rail.mjs` 15, `aliases_page.mjs` 9,
    `playlist_page.mjs --preview-only` 13, `scanner_pages.mjs` 40.
  - `scanner/doc/API_FIELDS.md` regenerated from A.
- **Not checked:**
  - an archive saved by a browser from unit A's own page: the page's download ran against the
    PC daemon, and A's downloads were the tool's;
  - a download over HTTPS (port 8443);
  - 7-Zip and Windows Explorer on the ZIP64 archive (Python's `zipfile` and Windows' `tar` read
    it);
  - a history near its 2 GB limit: its copy's time, and a card with no room for it;
  - a recording deleted by the retention while its archive was being made on A (the host test
    covers one gone from the card);
  - what a download costs live audio in a browser: no chunk was missed by the recorder, and
    nobody listened;
  - a phone's width.
