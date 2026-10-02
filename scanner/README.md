# scanner

The trunking scanner for the Fishball Z7020 (Zynq-7020 and AD9361): P25 Phase 1 and DMR Tier III
control channels decoded, their calls followed on the gateware's traffic lanes, voice decoded
(IMBE, AMBE+2), played live in the browser, recorded and kept in a history. It replaces
`p25-httpd` (change 076; the design and its status are in `doc/DESIGN.md`).

## Layout

| Module | What it owns |
|--------|--------------|
| `boot` | Arguments, start-up and shutdown, the state the API reaches |
| `hardware` | The P25 core's registers and DMA rings, the AD9361, DDC presets |
| `radio` | The tuner, the radio lease, the window planner, the sample and dibit streams |
| `protocol` | P25 and DMR: framing, FEC, control and traffic decoders, one event type |
| `trunking` | The live site, the control receivers, the follower, the call book, the lanes |
| `audio` | The voice codecs, the AGC, live audio per lane |
| `services` | Configuration, the event log, history, recordings, the scan, the clock |
| `api`, `ui` | `/api/v1` (one route table) and the web UI it serves |

## Build and test

```text
cargo test                      # host tests (the board code is built for Linux only)
cargo-zigbuild zigbuild --release --target armv7-unknown-linux-gnueabihf.2.31
```

The ARM build needs `cargo-zigbuild` and `ziglang`, for example from the repo's `.venv-hdl`.

## On a unit

The image starts it from its init script. By hand, with `p25-httpd` stopped and the SD card
mounted:

```text
/tmp/scanner --listen 0.0.0.0:8080
```

Its files:

- `/mnt/jffs2/scanner/`: `radio.json`, `systems.json` (each system with its aliases and
  sites), and `state/` (what the radio learns: crystal, band plans, grant counts). A unit
  without them starts empty.
- `/mnt/sd/scanner-history.sqlite`: the history.
- `/mnt/sd/p25_recordings/`: the recordings (`--recordings-dir` elsewhere), RAM without a card.

The API is in `doc/API.md`, generated from the route table.
