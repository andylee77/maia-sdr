# scanner

The trunking scanner for the Fishball Z7020 (Zynq-7020 and AD9361): P25 Phase 1 and DMR Tier III
control channels decoded, their calls followed on the radio core's traffic lanes, voice decoded
(IMBE, AMBE+2), played live in the browser, recorded and kept in a history. In ATSC mode it finds
the TV channels and names their stations. Its design is in `doc/DESIGN.md` (change 076); the
radio core it drives is in `../doc/changes/079_general_radio_core.md`.

## Layout

| Module | What it owns |
|--------|--------------|
| `boot` | Arguments, start-up and shutdown, the state the API reaches |
| `hardware` | The radio core's registers and DMA rings, the AD9361, DDC presets |
| `radio` | The tuner, the radio lease, the window planner, the lane streams |
| `dsp` | Filters and the shared multiply-accumulate runs (NEON on the board) |
| `protocol` | P25, DMR and ATSC: demodulators, framing, FEC, control and traffic decoders, PSIP, one event type |
| `trunking` | The live site, the control receivers, the follower, the call book, the lanes |
| `audio` | The voice codecs, the AGC, live audio per lane |
| `services` | The unit's mode, configuration, the event log, history, recordings, the systems scan, the TV scan, the clock |
| `api`, `ui` | `/api/v1` (one route table) and the web UI it serves |

## Build and test

```text
cargo test                      # host tests (the board code is built for Linux only)
cargo-zigbuild zigbuild --release --target armv7-unknown-linux-gnueabihf.2.31
```

The ARM build needs `cargo-zigbuild` and `ziglang`, for example from the repo's `.venv-hdl`.

## On a unit

The image starts it from its init script (`/etc/init.d/S60scanner`, logging to
`/var/log/scanner.log`). By hand, with the service stopped (wait until `pidof scanner` is empty)
and the SD card mounted (the stop unmounts it):

```text
/tmp/scanner --listen 0.0.0.0:8080
```

Its files:

- `/mnt/jffs2/scanner/`: `radio.json`, `systems.json` (each system with its aliases and
  sites), and `state/` (the unit's mode, and what the radio learns: crystal, band plans, grant counts). A unit
  without them starts empty.
- `/mnt/sd/scanner-history.sqlite`: the history.
- `/mnt/sd/p25_recordings/`: the recordings (`--recordings-dir` elsewhere), RAM without a card.

The API is in `doc/API.md`, generated from the route table.
