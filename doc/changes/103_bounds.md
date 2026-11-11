# 103 — Bounds on what a stalled or greedy client can hold

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. On unit A as a test build in RAM (`2026-11-11-bounds1`) over the SD
image `2026-11-10-pages2`, which does not have it.

## Why

095's review listed, for the roadmap's step 6, bounds the API lacked: "a recordings list without
a limit, a stalled CSV reader, sockets without a send timeout or a cap". 101 added one more of
the kind: a download is one at a time, and a client that stopped reading without leaving held
its place for good. None of them waits on the rest of step 6 (see "What is left of step 6").

## Changes

- **A download ends when its client takes nothing for a minute** (`util/handoff.rs`): its
  thread stops, its place is given up, and the next download can start. The radio's log says
  the client "left or stalled". Before, only a client that closed its connection ended it.
- **The history's CSV export** (`GET /api/v1/activity/calls?format=csv`) ends the same way: a
  reader that stalls no longer holds a thread and a connection to the history for good.
- **A socket whose client takes nothing for ten seconds is closed** (`/ws/live`, `/ws/audio`,
  `/ws/events`). Before, a page that hung held its task, and with it a place in each queue the
  radio pushes from.
- **At most 64 sockets are open at once;** the next upgrade is refused with 503. A page holds
  one, and one more while it listens.
- **`GET /api/v1/recordings` lists 200 recordings when no `limit` is given, and 2,000 at most.**
  Without a limit it listed every recording: 101,667 on unit A, built on one of the two workers
  that also run trunking and live audio. The pages always gave a limit; `total` and the stores'
  state are unchanged, and `/api/v1/files` has the recordings by span of time and as archives.

## What is left of step 6

102 built the passwords, the two levels, the tokens and the guard. Reading the firmware for the
last two answers (Q9, Q10) found what "as recommended" did not cover, so they wait for Andy:

- **Q9, the certificate.** The firmware's script (`S50p25-httpd-certificates`) makes a CA and a
  certificate at first boot and deletes the CA's key. So the unit cannot make a new certificate
  "from its own CA" when its address changes: that needs a new CA whose key stays on the unit
  (which a PC then installs once). And once port 8080 only redirects, every tool must speak
  HTTPS and trust that CA: all seven tools and the bench use `http://<unit>:8080` today.
- **Q10, `iiod`.** It is started inside the USB gadget's script (`S23udc`) as the gadget's
  FunctionFS daemon: stopping it can take the whole gadget down, the USB network with it, which
  is how unit B is reached. The unit has no firewall to close only its network port, `iiod` has
  no option to serve USB alone, and the bench (`fbench`) reaches each unit's `iiod` over the
  network. "Off unless switched on" is therefore a change to the gadget's script and to the
  bench, not a switch alone.

## Checks

See the status log for the counts.

- **Host tests:** a queue that is read takes everything; a client that stalls or leaves ends
  the maker; a download whose client reads nothing gives its place up and the next one starts.
- **`runs/ui_check/bounds_api.py`** (new), against a daemon on this PC: 64 sockets open and the
  65th refused, their places free again when they close; a socket that reads nothing closed by
  the daemon; the recordings' list with no limit and with a huge one; and with `--slow`, a
  download held and not read giving its place up after its minute.
- **The pages,** as before.

## Status log

- 2026-11-11: built. Host tests 571 pass (3 new); the DMR reference's tests pass (102, holding
  its floor of 24,984 lines); the ARM check and the ARM release build are clean. BUILD_TAG
  `2026-11-11-bounds1`.
- 2026-11-11: on the PC daemon: `bounds_api.py --slow` 5 steps (64 sockets and the 65th refused
  with 503; a socket reading nothing closed 13 s after it opened; a held download given up after
  60 s with "the client left or stalled" in the log, a second one refused meanwhile);
  `access_api.py` 32; `files_page.mjs` 15, `access_page.mjs` 15, `now_rail.mjs` 29 and 15,
  `aliases_page.mjs` 34, `playlist_page.mjs` 16, `scanner_pages.mjs` 40, `tool_pages.mjs` 31,
  `tv_watch.mjs` 12. The aliases check's step that failed in 102 passes: read from another
  daemon's history, a talkgroup heard since the page's own daemon's clock may show no age.
- 2026-11-11: `2026-11-11-bounds1` on unit A (binary only, in RAM over the SD image
  `2026-11-10-pages2`, which does not have it: a reboot of A returns to that image's pages):
  - live on Clay County within 30 s, open as before;
  - `GET /api/v1/recordings` lists 200 of its 101,744 recordings with no limit given, and 2,000
    when asked for 100,000;
  - `bounds_api.py --slow` against A, 5 steps, as on the PC;
  - a day's archive (2,979 recordings, 150.7 MB) in 41.2 s, as in 101: the hand-off costs
    nothing; the control channel at 100 % and no counter moved;
  - with A serving the page, reads only: `files_page.mjs --no-downloads` 9 steps,
    `tool_pages.mjs` 27, `now_rail.mjs` 15, `aliases_page.mjs` 9,
    `playlist_page.mjs --preview-only` 13, `scanner_pages.mjs` 40;
  - `scanner/doc/API_FIELDS.md` regenerated from A.
- **Not checked:** a CSV export whose reader stalls (the hand-off it uses is the download's,
  which was); a page in a real browser that hangs for ten seconds and comes back (it reconnects
  as after any drop: the store's own retry, from 090).
