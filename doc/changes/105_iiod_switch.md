# 105 — libiio's daemon off unless switched on

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. In unit A's SD image since `2026-11-11-access2` (106), with the
firmware's side, tezuka_fw's change 008. Unit A's libiio is off, the choice kept off.

## Why

The roadmap's Q10, the last part of step 6 but for SSH. `iiod`, libiio's daemon, lets any
program that can reach the unit on the network (port 30431) or its USB cable use the board as a
tuner: SDR++, SDRTrunk and GNU Radio do, and so does anything else, retuning the AD9361 under
the scanner with no check of who asks. Andy's answer to Q7 to Q10 ("Yes, as recommended")
included: off unless a switch in Settings turns it on. 103 read the firmware and found it is
not a daemon one can stop: it is the USB gadget's own (its FunctionFS function), started in the
gadget's script, and a gadget whose FunctionFS daemon leaves is taken off the bus whole, the
USB network with it; the unit has no firewall; `iiod` cannot serve USB alone; and the bench
works through it. Asked "Build iiod's switch as recommended: off unless switched on, the bench
turning it on?", Andy answered "Yes, build it".

## Changes

### The firmware's switch (tezuka_fw 008)

- **`/usr/sbin/iio-switch`** keeps the choice as one word on the flash
  (`/mnt/jffs2/iio-switch`; none there is off) and makes it so: `on`, `off`, `once` (on until
  the unit restarts, the choice kept unchanged), `status`, `wanted`.
- **The gadget's script (`S23udc`)** builds the gadget with or without its IIO function, and
  starts `iiod` or not, by that choice; and changes a running gadget: it leaves the bus, the
  function is linked or unlinked and the daemon started or stopped, and it returns. The other
  functions are not touched: the USB network keeps its interface and address.
- **Off** is no `iiod` at all, so nothing on port 30431, and no IIO function on the USB cable.
- The validation image (the same root, the hwval core) has no scanner and is a bench's: there
  it is on at every start. The other images that share the script are unchanged.

### The scanner

- **`services/iiod.rs`** asks the firmware's command and tells it (`--iio-switch`, by default
  `/usr/sbin/iio-switch`). A system without the command (a PC, an image from before it) has no
  switch. The scanner does nothing about it at its own start: the firmware's start has.
- **`GET /api/v1/access/iiod`**: `available` (the system has the switch), `wanted` (the choice
  kept), `running` (it runs now). **`PUT /api/v1/access/iiod`** with `on` switches it and keeps
  the choice; `until_restart` with `on` leaves the choice kept as it is. Both are the
  administrator's, like the rest of `/api/v1/access`; on an open radio every client is that.
  Each switch is a line in the radio's log.
- **The Access card** has a part "The board as a tuner (libiio)" for whoever may switch it: the
  switch (the choice kept), what it lets in and that switching takes the board off its USB
  cable for a moment. When it runs without being kept on, the card says so ("It runs now, until
  the radio restarts: a tool switched it on") with "Stop it now"; when it is kept on and does
  not run, it warns. On a firmware without the switch it says there is none and that the daemon
  runs always. The card's parts are now in this order: the passwords, the tokens, the browsers,
  the names, libiio, HTTPS, and last "Open the radio again".

### The bench

- **`fbench` switches it on when it needs it** (`fbench/services.py`): the first time a run
  asks for libiio on a unit and finds its port closed, it has the unit switch it on until the
  unit restarts. It asks the scanner's API (an open unit, or an administrator's token in
  `SCANNER_TOKEN`); with the scanner stopped or refusing, it asks over SSH (`iio-switch once`,
  started apart from the session, which a unit reached over USB would drop). Then it waits for
  the port.
- **SSH before the card is known.** The bench keeps one host key for each SD card and learns
  which card is in a unit over libiio, which is what is off: it tries each key it has on record
  for the unit. A card it has never met with libiio on cannot be reached that way; the bench
  then names the unit in a warning (switch it on in its Settings).
- **An answer lost with the link.** A unit reached over its USB cable is off the bus while it
  switches, and the request with it: no answer is not taken for a refusal, and the port is
  waited for.

### The checks' tooling

- The PC daemon is started with `--iio-switch runs/ui_check/fake_iio_switch.cmd`, a stand-in
  for the firmware's command whose state is a file beside it.
- **`runs/ui_check/iiod_api.py`** (new) tries the switch through the API; with `--unit` it runs
  against a unit, where it also tries the port and reads the board through libiio. It sets no
  password and leaves the switch as it found it. `access_api.py` has the new route among its
  doors, and `access_page.mjs` works the switch through the card.

## What it costs

- **A unit is off until someone switches it on**, with the first image that has this. SDR++,
  SDRTrunk or GNU Radio using the board as a tuner need it on.
- **Switching takes the unit off its USB cable for a moment**: a host there loses the unit's
  network and serial console and finds them again. Over Ethernet nothing is felt.
- **The bench leaves it on** until the unit restarts, on any unit it worked with.

## Not in this change

- **SSH**, which is Andy's to say (keys only, or as it is). The bench and this switch's SSH path
  use keys.
- Who is connected to `iiod` (the roadmap's idea of showing it); a firewall; the bench off the
  legacy `/api/*` routes.

## Checks

See the status log for the counts.

- **Host tests:** the switch read from what the firmware prints; a system without the command.
- **The bench's host tests** (`tests_host/test_services.py`, new): a daemon that answers left
  alone; one found off switched on through the scanner; over SSH when the scanner refuses; each
  recorded key tried; a unit without the switch not waited for; a lost answer; a daemon that
  does not come up given up.
- **`iiod_api.py`**, on the PC daemon's stand-in and on unit A's real switch.
- **The firmware's scripts on unit A by hand** (tezuka_fw 008's record).
- **The pages,** as before.

## Status log

- 2026-11-11: built. Host tests 580 pass (2 new); the DMR reference's tests pass (102); the ARM
  check and the ARM release build are clean; the bench's host tests pass (277, 8 new).
  BUILD_TAG `2026-11-11-iiod1`.
- 2026-11-11: on the PC daemon (started with the stand-in switch), each run leaving it open and
  the switch off:
  - `iiod_api.py` 10 steps: off, on, on again, on until the restart, stopped; what it cannot
    mean refused; a daemon that does not start told with nothing claimed; the log's lines; left
    as found;
  - `access_api.py` 33 (nobody and a listener are refused the switch); `access_page.mjs` 20
    steps at two sizes over HTTPS, five of them the switch through the card: off at first,
    switched on, the state a tool leaves with "Stop it now", stopped, a switch that does not
    take told and gone back;
  - `https_api.py` 16, `bounds_api.py` 4; `tool_pages.mjs` 31 at two sizes and over HTTPS,
    `files_page.mjs` 15 and over HTTPS, `playlist_page.mjs` 16, `aliases_page.mjs` 34,
    `now_rail.mjs` 29 and 15, `scanner_pages.mjs` 40, `tv_watch.mjs` 12, `modules_check.mjs`
    47 modules.
- 2026-11-11: on unit A (the binary, and the firmware's `S23udc` and `iio-switch`, in RAM over
  the SD image `2026-11-10-pages2`: a reboot of A returns to that image, where `iiod` runs
  always):
  - the firmware's switch by hand first (tezuka_fw 008's record): off in 0.3 s, on in 0.5 s,
    the gadget back on the bus each time with `usb0` at its address, the scanner live at 100 %
    throughout;
  - `iiod_api.py --unit` 10 steps through the scanner's API: with it off port 30431 refuses;
    with it on `iio_attr -u ip:192.168.120.50 -C` reads the board; left off as found;
  - the bench's own code against A (a copy of its configuration with A's Ethernet address,
    since the bench's is A's USB one and the cable is not in): found off, switched on until the
    restart through the scanner's API and the board's context read 3.7 s later, the card's
    serial with it; a second run found it on at once. Before the scanner there had the route,
    the same step fell to SSH and was refused for the host key: the bench has no key on record
    for the card now in A, which is the case the warning names;
  - A's own page then showed it: the switch off, "It runs now, until the radio restarts: a tool
    switched it on (the bench does)", "Stop it now" (`runs/ui_check/t105A_settings.png`);
  - with A serving the page, reads only, over HTTP: `tool_pages.mjs` 27 at two sizes,
    `files_page.mjs` 9, `now_rail.mjs` 15, `aliases_page.mjs` 9,
    `playlist_page.mjs --preview-only` 13, `scanner_pages.mjs` 40; over HTTPS:
    `tool_pages.mjs` 27, `files_page.mjs` 9, `now_rail.mjs` 15;
  - left with libiio off, the choice kept off, the scanner live on Clay at 100 %, open as
    before; `scanner/doc/API_FIELDS.md` regenerated from A.
- 2026-11-11: in the SD image `2026-11-11-access2` on unit A (106), **the gadget built at
  start by the choice**, which the test build could not show:
  - kept off, A started with no `iiod`, port 30431 refusing, and its gadget on the bus with the
    network, storage and serial functions and no IIO function;
  - kept on through the API and restarted, A started with `iiod` running and its gadget built
    with the IIO function, and `iio_attr` read the board from the PC;
  - `iiod_api.py --unit` 10 steps on the image's own scripts; then switched off and left so.
- **Not checked:**
  - **a host on the USB cable while it is switched**: A's USB port has power only and unit B
    was off. That is where the bench's own link to A is (192.168.2.1), and what the
    lost-answer handling is for: its logic is in the bench's host tests, not on a cable;
  - the bench's SSH path on a unit (refused on A for the host key, as above), and so a unit
    with the scanner stopped;
  - the validation image, where the firmware has it on at every start;
  - the switch pressed in a browser on unit A (the page check that presses it sets passwords
    and runs on the PC daemon; on A the API switched it and the page showed it);
  - a closed unit: an administrator's token switching it, a listener's refused (the PC daemon,
    with sessions).
