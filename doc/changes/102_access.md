# 102 — Access: a password, two levels, tokens, and a guard on every door

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. On unit A as a test build in RAM (`2026-11-11-access1`) over the SD
image `2026-11-10-pages2`, which does not have it. Unit A is open: no password was set on it.

## Why

The roadmap's step 6, its first part. Until now anyone who could reach the unit could listen to
it and change it, a factory reset included; a web page on any site could open the unit's sockets
in a visitor's browser and read its state and audio; and a name pointed at the unit by someone
else's DNS passed the one check there was. Asked "Build Access next (step 6) with the roadmap's
recommended answers to Q7 to Q10?", Andy answered "Yes, as recommended" (2026-11-11):

- **Q7:** the unit is a LAN appliance: other people and devices on the network, and web pages in
  a browser, are kept out; reaching it from outside is through a VPN of his.
- **Q8:** an administrator (everything) and a listener (see and hear, change nothing), plus
  tokens for tools and the panel.
- **Q9:** once a password is set, port 8080 only sends the browser to HTTPS. Not in this change.
- **Q10:** `iiod` off unless a switch turns it on. Not in this change. For SSH the roadmap gave
  no recommendation: it is still his to say.

## Changes

### Open until a password is set

- **A unit with no administrator's password is open,** as a new unit is and as every unit was:
  each client is the administrator. Nothing changes for a unit until its password is set.
- **The first administrator's password closes it.** From then every route and socket asks who
  the client is. Whoever sets it is signed in by it.
- **Removing the administrator's password** (it needs that password) removes every password,
  session and token, and the unit is open again.

### Who a client is

| Client | How it is known | What it may do |
|--------|-----------------|----------------|
| Nobody | — | Load the page (it holds no data), ask who it is, sign in |
| A listener | The listener's password, or a token at that level | Every read and every socket: the pages, live audio, recordings, the history, downloads |
| The administrator | The administrator's password, or a token at that level | Everything |

- **A browser signs in with a password** (`POST /api/v1/session`); which password it is decides
  the level. Its session is a cookie the page's scripts cannot read (`HttpOnly`), sent only to
  this unit from its own pages (`SameSite=Strict`), and over TLS only when it was set over TLS.
- **A tool carries a token** (`Authorization: Bearer fbs_…`), made by the administrator in
  Settings and shown once. A read may carry it in its address instead (`?token=`), for a player
  that sends no header: VLC on a TV stream. A tool that drives a page trades its token for a
  session (`POST /api/v1/session` with `token`).
- **A listener changes nothing:** each write is refused with 403 and "signed in as a listener:
  this is the administrator's", which the pages show. The passwords, sessions and tokens are the
  administrator's to see too.

### What is kept, and how

- **A password is kept as a salted PBKDF2-HMAC-SHA256 hash** of 100,000 rounds: about a second
  of the A9 for each password tried, on a thread off the runtime. A session and a token are 32
  random bytes, kept as their SHA-256. The functions are aws-lc's, the library the unit's TLS
  already uses.
- **One file on the flash,** `scanner/access.json`, outside the configuration: an export does
  not carry it, an import does not replace it, and a factory reset leaves it, as it leaves the
  certificates and the crystal's calibration: they are the board's.
- **Sessions do not run out by the clock,** because the board has none of its own (it starts in
  1970 and is set by the site, the internet or a browser). A session ends when it is signed out,
  when its password changes (but for the browser that changed it), when the administrator ends
  it, or when 32 newer ones push it out.
- **A socket ends with what it was opened by:** `/ws/live`, `/ws/audio` and `/ws/events` close
  when their session is signed out or their token revoked.
- **Wrong passwords:** after five in a row the next try waits 30 seconds, the right one too.
- **A lost password** needs the unit in hand: an empty file named `scanner-reset-access` on the
  card clears the passwords, sessions and tokens at the next start and is removed; or, from the
  serial console or SSH, removing `/mnt/jffs2/scanner/access.json`.
- **The radio's log** (kind `access`) tells of each password set, changed or removed, each
  sign-in, each wrong password, each token made or revoked. It holds no secret.

### The doors that were open with or without a password

- **A socket from a page of another origin is refused** (`Origin` against `Host`, at the
  upgrade), as a write already was. A page on another site can no longer open `/ws/live` or
  `/ws/audio` in a visitor's browser.
- **A request sent to a name that is not the unit's is refused** (421): the unit answers to its
  addresses, `localhost`, its own host name and that under `.local`, and names its administrator
  adds. A page whose name was pointed at the unit by its own DNS gets nothing.

### The pages

- **A radio with a password shows its sign-in page first,** and nothing else until it is done.
  A browser signed out elsewhere, or by a changed password, goes back to it.
- **The header says who is signed in,** with Sign out. An open radio shows neither.
- **Access** is the first card of the radio's Settings. On an open radio it says so and offers
  the first password. To the administrator it holds: the administrator's password; a listener's
  (set, change, remove); tokens for tools (made, shown once, revoked); the browsers signed in
  (one ended, or all the others); the names the radio answers to; and opening the radio again.
  A listener's card says they are one.

### Tools and checks

- **`SCANNER_TOKEN`** in the environment is the token every tool sends: `tools/scanner_token.py`
  (for `api_fields.py`, `atsc_check.py`, `atsc_live_check.py`, `burst_timing.py`,
  `scanner_files.py`, `scanner_live_check.py`, `sdrtrunk_playlist_reference.py`), and the bench's
  HTTP client (`bench/fbench/transport.py`). With none set nothing changes.
- **The page checks** (`runs/ui_check/unit.mjs`): with a token their browser is signed in by it.
  A check that reads a unit from a page the PC served (`--reads`) did so through the sockets'
  open door; it now goes through a door of its own on the PC, which hands each request on
  without the page's origin and with the token.

### Also fixed

- **Settings' cards were not given the way to read the page again** (100): after a delete of
  recordings the Scanner's Settings called nothing. No check presses a delete.

## Not in this change (the rest of step 6)

- **HTTPS as the one way in** (Q9): port 8080 sending the browser to HTTPS once a password is
  set, and the certificate remade when the unit's address changes. Until then a password set
  over port 8080 crosses the network in the clear, as everything does today.
- **`iiod`** (Q10): still open to the network. **SSH:** as it is; Andy's to say.
- **The bench off the legacy `/api/*` routes:** they are behind the guard like the rest, and
  stay.
- **A listener's page with its controls taken away:** it shows them, and the radio refuses.
- **The stream's address on Watch** does not say that a closed radio wants a token in it.
- **The bounds 095's review listed** (a recordings list without a limit, a stalled CSV reader,
  sockets without a send timeout or a cap).

## Checks

See the status log for the counts.

- **Host tests:** an open unit and what closes it; a password signing in at its level; nothing
  secret in the file; a changed password ending its sessions but the changer's; tokens; sessions
  ended; the wait after wrong passwords; the unit's own names; the card's marker; each route's
  level; a client's cookie and token read from its headers; a token in the address taken out of
  it; the cookie's attributes.
- **`runs/ui_check/access_api.py`** (new, a daemon on this PC only: it sets passwords): every
  door as nobody, as the listener, as the administrator and with tokens, routes and sockets;
  a socket closed when its session or token ends; the log; and the daemon left open.
- **`runs/ui_check/access_page.mjs`** (new, the same): the card on an open radio; the password
  set through it; a listener's password and a token made; signed out to the sign-in page; a
  wrong password; in as the listener, who changes nothing; in as the administrator; the radio
  opened again through the card.
- **With the PC daemon closed** (`runs/ui_check/locked_run.py`): a tool and the page checks with
  a listener's token.
- **The other pages,** with the radio open, as before.

## Status log

- 2026-11-11: built. Host tests 568 pass (13 new: the access service's ten, the guard's three);
  the DMR reference's tests pass (102, holding its floor of 24,984 lines); the ARM check and the
  ARM release build are clean; the bench's host tests pass (269). BUILD_TAG
  `2026-11-11-access1`. `modules_check.mjs` 47 modules.
- 2026-11-11: on the PC daemon, each run leaving it open:
  - `access_api.py` 32 steps;
  - `access_page.mjs` 15 steps at 1920x1080 and at 1280x800. Its first runs failed in the check
    itself (a toast read after it had gone; a page not loaded afresh), not in the page;
  - closed, with a listener's token in `SCANNER_TOKEN`: `scanner_files.py` answered;
    `files_page.mjs --no-downloads` 9 steps, its browser signed in by the token;
    `tool_pages.mjs` reading through the door 26 steps, sockets included;
  - open, as before: `files_page.mjs` 15, `now_rail.mjs` 29 with the writes and 15 with unit A's
    reads through the door, `playlist_page.mjs` 16, `scanner_pages.mjs` 40, `tool_pages.mjs` 31,
    `tv_watch.mjs` 12; `aliases_page.mjs` 33 of 34: the first talkgroup heard on unit A showed
    no age, because A's clock is 32 s ahead of the PC's and it had been heard seconds before (a
    page from one daemon reading another's history; from 097).
- 2026-11-11: `2026-11-11-access1` on unit A (binary only, in RAM over the SD image
  `2026-11-10-pages2`, which does not have it: a reboot of A returns to that image's pages):
  - live on Clay County within 35 s; **open, as it was:** `GET /api/v1/session` answers
    `protected: false`, and no `access.json` was written to its flash. No password was set on
    unit A: that is Andy's to do;
  - a request sent to another name got 421; one sent to `pluto.local`, its own host name, 200;
    port 8443 answers as 8080 does;
  - with A serving the page, reads only: `files_page.mjs --no-downloads` 9 steps,
    `tool_pages.mjs` 27 at both sizes (Settings begins with the Access card), `now_rail.mjs` 15,
    `aliases_page.mjs` 9, `playlist_page.mjs --preview-only` 13, `scanner_pages.mjs` 40;
    `scanner_files.py` counted a selection;
  - `scanner/doc/API_FIELDS.md` regenerated from A.
- **Not checked:**
  - a password on unit A, and so the sign-in page, a listener and a token there: no check sets
    a password on a unit. Everything closed was checked on the PC daemon;
  - signing in over HTTPS in a browser (the cookie's `Secure` is a host test);
  - the card's `scanner-reset-access` on a unit (a host test covers it);
  - a player with a token in a stream's address (VLC on `/api/v1/atsc/stream/{rf}?token=`): a
    read with its token in the address was checked on the status, the events and a socket;
  - the bench and the other tools against a closed unit (`scanner_files.py` was);
  - how long a password takes to check on the A9: 100,000 rounds were reckoned at about a second
    from `openssl speed` on A (12 MB/s of SHA-256 at 64 bytes), not timed in the daemon;
  - a phone's width.
