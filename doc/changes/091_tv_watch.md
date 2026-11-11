# 091 — TV: the Watch page with three views, and Channels in the rail

**Date:** 2026-11-10. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** see the status log.

## Why

After the rail and the new Now page (090) Andy asked for the TV pages next, as drawn in the
options' A2 (a channel list, the picture, the signal). While it was built he added that he also
liked the launcher option's TV screen (the picture across the page with the channels below), and
asked to cycle between the small view, the large view and a full-screen player with nothing over
the picture.

The old Viewer was a channel list in a drop-down above a text block: the live receiver's state
was one sentence, it asked the radio for that state every second (the brief says no page polls),
the TV's sound had no volume, and Play was refused with a message while the scanner had the
radio.

## Changes

### Watch (`views/watch.js`, replacing `views/viewer.js`)

TV's first page, at `#watch`.

- **The channels:** every virtual channel the last TV scan named, by number, each with its RF
  channel and the MER the scan read. A click picks one; while a channel plays, the picked one
  plays in its place. Two buttons step to the channel before and the next.
- **The picture** with one round Play or Stop, the channel's name, and what plays (live, or its
  recording). Until a picture is up the channel's name and the player's state lie over it.
- **Play takes the radio.** When the scanner has the radio, Play asks, gives the radio to TV,
  has the unit tune the channel, then plays. Another channel of the RF channel the receiver is
  already on plays from the stream as it runs, with no new tuning. Without the radio core's
  8-VSB block a recording of the RF channel on the card plays, as before.
- **The programme:** its streams as the scan's capture read them, and the stream's address for
  VLC or another player; "Open in VLC" saves a playlist.
- **The signal:** while the channel plays live, the receiver's MER with its last fields as bars,
  the carrier offset, the timing, the codewords, those corrected and those beyond Reed-Solomon,
  the equalizer's solves and the players on the stream; otherwise what the scan read (MER,
  carrier to noise, pilot, power, packets).
- **Three views,** cycled by the View button or a double click on the picture:
  - **small:** the channels down the left, the picture in the middle, the signal on the right;
  - **large:** the picture across the page, the channels a strip of chips below it;
  - **full screen:** the picture alone, nothing over it. A double click goes on to the small
    view; Esc leaves full screen for the view it came from.

  The view in the page is kept per browser, as the picked channel is.
- **Stopping:** Stop, or leaving the page, stops the unit's receiver too unless another player
  (VLC, another browser) is on its stream. A receiver left running for another player shows on
  the page with its own Stop.

### Channels

- **Scan takes the radio** for TV first, when another module has it (it asks).
- A named station's virtual channels each have **Watch**, which opens Watch on that channel.
- It is TV's second tab.

### Shared

- **Live TV's state is pushed.** `/ws/live` takes `atsc_live` in a page's subscription and sends
  `{"type":"atsc_live","live":{…}}` (what `GET /api/v1/atsc/live` gives) when it changed, looked
  at each second. Watch asks the radio for nothing while it plays.
- **One volume.** The header's volume sets the TV's sound too (088's rule), and stays in the
  header in every module. Listen, Level and Alerts only still show only while the scanner has
  the radio.
- **`js/mode.js`** holds the radio's change of module (asking first when the scanner would
  stop), for the shell and for the pages that need the radio.
- A page that fills the screen says so in `js/modules.js` (`fill`); Now and Watch do.

## Not in this change

- **The scan's results are kept in memory only:** after a restart of the scanner Watch has no
  channels until a TV scan runs again (about two minutes, with the radio).
- Channels keeps its layout.
- The player crate's header (`scanner/player/src/lib.rs`) still says "Viewer page": a change
  there means building its module again.
- A programme guide (the stations send one in PSIP; nothing reads it yet), captions, a second
  audio language, recording a programme.

## Checks

- **`runs/ui_check/modules_check.mjs`:** 35 modules, syntax and imports.
- **Headless Chrome** (`runs/ui_check/tv_watch.mjs`, sound muted), the page from the PC daemon
  (`2026-11-10-tv1`) with a saved TV scan in the page's store (`runs/atsc/20261004_164849`) and
  two recorded streams in its card folder: 12 steps, no exceptions:
  - TV opens on Watch; 22 channels listed by number;
  - 47.1 picked shows its programme and the scan's signal; Play gives a picture from the
    recording; the header's volume is the player's gain;
  - the large view puts the channels under the picture and still fits the screen; the next
    channel plays in its place; full screen has nothing over the picture; a double click goes on
    to the small view;
  - 30.1, on another RF channel, picked while playing, plays; Stop stops.
- **Host tests:** 516 pass.

## Status log

