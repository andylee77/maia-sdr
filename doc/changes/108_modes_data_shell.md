# 108 — The modes made general, and Data mode's shell

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. In unit A's SD image, `2026-11-12-decode1`, since 2026-11-12
(its record is in `110_oregon_v1_decoder.md`).

## Why

The roadmap's step 7, Data mode, begins. Before it comes "the modes made general (three and
more; who has the radio)" and the `packet_data` rename; the study's own step for both is its
step 2, "The mode" (`scanner/doc/DATA_MODE.md` §9). Asked "Start the roadmap's step 7, Data mode,
with making the modes general first?", Andy answered "Yes, start it".

The unit had two modes, and the change of mode knew it: entering ATSC meant leaving the scanner
and the reverse, the radio's lease was named for ATSC, and a message said "TV has the radio"
whenever a mode did. A third mode cannot be shown to work with two, so this change adds the
third as far as the study's step 2 takes it: Data mode itself, with its module and its three
tabs empty.

## Changes

### The change of mode (`services/mode.rs`)

- **A mode other than the scanner's holds the radio** (`Hold`: the lease, and the site the
  scanner brings back). The scanner's is the radio's own state: its live site follows calls.
- **Leaving a mode gives its hold back; entering one takes a hold.** From the scanner the hold
  is made (the lease taken, the live site paused); to the scanner it is dropped (the gain set
  back, the site live again); **from one mode to another it is handed on**, so the radio is
  never free between them and no scan or site switch can take it there. Before, ATSC to
  anything but the scanner had no path.
- **The lease's state for this is `mode`** (it was `atsc`): `status.lease` says the radio is a
  mode's, and `status.mode` says whose. The site's log line names the mode ("data mode has the
  radio: the live site is paused"), and so does the refusal to replace the configuration while
  a mode has the radio.
- **The start** brings back whichever mode the unit was in, not ATSC alone.
- A new mode is a variant with its label, its service's `enter` and `leave`, and their arm each.

### Data mode's shell

- **`Mode::Data`** (`data` in the API and on the flash, "Data" on the pages), and
  `services/data.rs`: it takes the hold and gives it back. Nothing listens in it yet; the scan,
  the devices and the captures are the study's steps 3 to 5.
- **Its module in the rail,** between TV and the radio's own: Data, with the tabs Scan, Devices
  and Captures, each saying what it will hold and that it is not built. The header reads "Radio
  on Data · nothing listens yet". On its pages while another module has the radio, the notice
  says what the mode will do and that so far it only takes the radio.
- The shell of the pages needed nothing: the rail, the notice, "Switch the radio to …" and
  "Return to Scanner" were built from the table of modules (090). The scanner is asked about
  only when the radio is taken from it.

### Packet data takes its own name

`data` is the mode's. P25 packet data's route is **`GET /api/v1/packet_data`** (it was
`/api/v1/data`), and its lines in the scanner's log have the class `packet_data` (it was `data`),
as its service and its page were already named. Nothing reads the old names.

## Not in this change

- **The study's steps 0 and 1,** which come before its step 2 in its own order and need Andy:
  the RTL-SDR's day of ground truth, and the wide lanes proven on the bench (B wired into A) or
  from the ESP32-DIV. Neither is needed by the mode's shell.
- Anything that listens: the band plan, the scan, the burst detector, the data log, captures,
  the waterfall, decoders, and their routes (`/api/v1/data/...`).

## Checks

See the status log for the counts.

- **Host tests:** the modes' names; a round trip through all three modes, with the lease
  watched so that it is never given up between two of them, the site to come back to carried
  through, a mode refused while a scan has the radio (and the site not paused for it), and a
  start in another mode than the scanner's.
- **`runs/ui_check/data_mode.mjs`** (new): the radio given to Data, to TV, to Data and back to
  the scanner through the shell's own controls, with what each page says meanwhile.
- **The pages,** as before.

## Status log

- 2026-11-11: built. Host tests 583 pass (1 new, and the mode round trip rewritten for three);
  the DMR reference's tests pass (102); the ARM check and the ARM release build are clean.
  BUILD_TAG `2026-11-11-modes1`.
- 2026-11-11: on the PC daemon: `data_mode.mjs` (new) 13 steps at two sizes: the rail's three
  modes; Data's pages readable while the scanner has the radio, with the notice and its
  switch; the radio given to Data (asked first, the site paused with its way back kept), the
  header's "Radio on Data · nothing listens yet" and "Return to Scanner"; handed to TV and back
  with nothing asked and the site still paused; a mode that is none refused; back to the
  scanner with its site as it was; the radio's log telling of each change. `tool_pages.mjs` 31
  at two sizes, `files_page.mjs` 15, `playlist_page.mjs` 16, `aliases_page.mjs` 34,
  `now_rail.mjs` 29, `tv_watch.mjs` 12, `access_page.mjs` 20 over HTTPS, `modules_check.mjs`
  48 modules. Two checks expected the rail without Data and were given it.
- 2026-11-11: `2026-11-11-modes1` on unit A (binary only, in RAM over the SD image
  `2026-11-11-ssh1`: a reboot of A returns to that image, which has two modes):
  - **the study's gate for its step 2, a mode round trip on A:** `data_mode.mjs` with A serving
    the page, 13 steps. The radio's log: data mode, ATSC TV mode 48 s later, data mode, scanner
    mode. The scanner's log: "data mode has the radio: the live site is paused" once, and the
    site live again 2 min 13 s later, on Clay County at 100 %; the site was not brought back
    between the three modes;
  - with A serving the page, reads only, over HTTP: `scanner_pages.mjs` 40 steps (the packet
    data by its new route), `tool_pages.mjs` 27, `now_rail.mjs` 15, `files_page.mjs` 9,
    `aliases_page.mjs` 9, `playlist_page.mjs --preview-only` 13; over HTTPS: `tool_pages.mjs`
    27, `now_rail.mjs` 15;
  - `scanner/doc/API_FIELDS.md` regenerated from A.
- **A device to test with** (Andy, as this was built): an Oregon Scientific outdoor temperature
  sensor that unit A should hear, on the air all day. It is written into the study's section 12.
- **Not checked:** a start of the unit in data mode (the host test starts one there; A was not
  restarted in it); TV actually scanning or playing between two visits to Data (the modes were
  entered and left with nothing running); a third browser following the change (the pages
  learn of the mode from the status pushed each second, as before).
