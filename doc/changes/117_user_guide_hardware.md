# 117 — A user guide with pictures, the README, and the hardware design in the tree

**Date:** 2026-11-12. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally. Documents and pictures only: nothing in the scanner changes, and
unit A's image stays `2026-11-12-range1`.

## Why

Andy, 2026-11-12: "I'd like to update the github readme to include a user guide with images of
the different pages. I also created a radio wiring and case design that we can add to the
project and link from the main readme."

## Changes

### The user guide (`doc/USER_GUIDE.md`, new)

- **Every page, in the rail's order,** with what it is for and how it is used: opening the
  unit and the one radio the modules share; the Scanner's Now, Activity, Systems, Aliases,
  Diagnostics and Settings; TV's Watch and Channels; Data's Scan, Devices and Captures; the
  Analog receiver; Aircraft's table and map; the radio's own Diagnostics, Files and Settings;
  and Access.
- **A picture of each page** but Access (`doc/guide/img/`, 18 files, 3.0 MB): 17 taken from
  unit A on the image `2026-11-12-range1`, each while its module had the radio, 1280 pixels
  wide, in the dark theme; and the map with OpenStreetMap's tiles, which is the one taken on
  the PC daemon in 115.
- **What the pictures do not show, on purpose:**
  - **The Access card.** On unit A it says that no password is set and names the unit's
    addresses and its CA's fingerprint. The picture of the radio's Settings is its right-hand
    column alone, and Access is described in words.
  - **The unit's place.** It has none. The aircraft's distances and the map's range rings are
    shown from a made place (an airport's reference point), put through the API for the
    pictures and taken away again.
  - **A television picture.** Watch is shown with a channel picked and nothing playing: a
    frame of a broadcast is the broadcaster's.
- **What they do show,** since it is what the pages are: the systems, talkgroups, aliases and
  radio numbers of the public-safety systems unit A listens to, with a day's call statistics;
  the control channel's messages in the Scanner's Diagnostics; three weather sensors near
  unit A with their readings; the aircraft heard for a minute.

### The README

- **Brought up to what the scanner is:** the five modules (it said two, and Data "a study so
  far"), the aliases and imports, live television, access by password over HTTPS; the radio
  core's 8-VSB receiver in "How it is split".
- **Links at its top** to the user guide, the hardware design, the roadmap and the change log;
  five of the guide's pictures; the two cases and the wiring diagram.

### The hardware design (`hardware/`, added as it was made)

Made in another session at Andy's asking and added here unchanged: 52 files, 9.0 MB. Its own
README is its record; in short:

- **The wiring** (`hardware/wiring/`): one harness for both units, drawn by a script: the power
  chain (charge port, charger with a power path, a latching switch, a 5 V booster), a Raspberry
  Pi Pico as the panel's controller with a 2.0 inch LCD, two knobs, four soft keys, a power
  button and a status light, a serial link and an "alive" wire to the board's JP5, and an I2S
  amplifier and speaker.
- **Two cases** (`hardware/cases/`, FreeCAD scripts with their STEP, STL and preview files): a
  handheld, 75 x 170 x 37 mm, and a desk unit in the shape of a mobile transceiver in a tilt
  bracket, from the same bought parts.
- **Paper and CAD only:** nothing has been wired, printed or measured, and sizes marked
  "assumed" in its README are to be measured first. It needs, before a harness does anything:
  the bitstream's UART and I2S pins on JP5, the daemon's own listening path and the panel's
  service, and the Pico's firmware. None of those exists.
- It answers the roadmap's Q13 as far as Andy has said (the Pico, two knobs with four soft
  keys, a power button and one light, the LCD, two 18650 cells, I2S from the PL), and replaces
  the parts of `PANEL_ADDON_BOARD.md` it names.

### With it

- **`runs/ui_check/guide_shots.mjs`** (new, with the page checks): takes the guide's pictures
  from a unit, giving the radio to each module in turn and putting back the mode, the
  aircraft's link, the analog receiver's tuning and the radio's place as it found them.
- **The roadmap:** step 9 and Q13 say the hardware design; the note on pushes says through 116.

## Not in this change

- **A picture of Access,** the sign-in page, or a television picture.
- **Pictures in the light theme,** or of a narrow screen.
- **The hardware built:** see its README's "Measure first" and "What it needs from the
  gateware and the daemon".
- **`PANEL_ADDON_BOARD.md` rewritten** to the new design: the hardware README says what
  changed from it.

## Checks

- Every link and picture named in the README and the guide exists in the tree; the Markdown
  lints clean.
- Each picture was looked at before it was added, for what it shows of the unit and its
  network.
- While the pictures were taken no request left the unit's own address (the script lists any):
  no tile was fetched.

## Status log

- 2026-11-12: the pictures taken from unit A (`runs/ui_check/guide_shots.mjs`), in three
  runs: the nine pages that need no change of mode; then TV, Data, Analog and Aircraft with
  the radio given to each; then Data's Scan and Devices again after two and a half minutes of
  listening, the first having been taken before a burst had come. The radio was off the
  scanner's site for about a minute at 23:52 UTC and three minutes from 23:53. The mode, the
  aircraft's link, the analog receiver's tuning and the radio's place (none) were as before
  after each run. No request left the unit's own address.
- 2026-11-12: each picture looked at. The Settings picture was first the whole page, with the
  Access card (no password set, the unit's addresses, its CA's fingerprint): it was taken
  again as the right-hand column alone. Nothing else was cut.
- 2026-11-12: the other session that made `hardware/` said it was finished and that every
  file git would add was meant to be: added as it was, nothing in it changed here.
- 2026-11-12: the README's, the guide's and the hardware README's links and pictures all
  resolve; the Markdown lints clean.
- **Seen and not changed:** the Scanner's Diagnostics picture shows the control channel's
  messages as they came, radio numbers among them and one encrypted call's key number and
  message indicator, which is what that page is for. Andy may want another moment of it.
- **Not checked:** how GitHub lays the README's two tables of pictures out; the guide read by
  someone who has not used the pages.
