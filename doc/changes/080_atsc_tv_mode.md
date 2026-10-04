# 080 — ATSC TV mode: the unit's mode, and the TV channel finder

**Date:** 2026-10-03. **Branch:** `080-atsc` (worktree `maia-sdr-080`), from fishball-p25
7acca18; not merged. **Bake required:** NO. The scanner only; the gateware is unchanged.

## Why

Andy wants the unit to do one thing at a time, chosen by its mode: the scanner (P25 and DMR
trunking) with its tabs, or ATSC TV with an ATSC tab. The first part of ATSC mode is finding the
TV channels on the air and how well each is received. Decoding them comes later.

## What

### The mode (`services::mode`)

- The unit is in `scanner` or `atsc` mode, kept in `state/radio.json` across restarts.
- ATSC mode takes the radio lease (`Lease::Atsc`) for as long as it lasts. The live site pauses
  (`LiveState::Away`, with the site to come back to) and every service that waits for a normal
  lease stands down: the crystal tracker, the site clock, the recentre, site switches and the
  systems scan.
- Back in scanner mode a TV scan in progress stops first, the configured receiver gain is set
  again and the paused site goes live. A unit that starts in ATSC mode holds the configured
  site for later.
- `GET`/`PUT /api/v1/mode`; `status.mode`; `status.lease` can be `atsc`.

### The channel finder (`protocol::atsc`)

- **Plan:** the US channels after the repack, RF 2-36. RF 2 and 3 are below the AD9361's 70 MHz
  LO, so a scan reads RF 4-36.
- **Windows:** 16 MSPS, two adjacent channels a window, with the LO 0.5 MHz above their middle.
  That keeps the DC spur off every pilot and off the channel edges where the floor is read. RF 4
  sits alone at the 70 MHz LO. 33 channels take 18 windows.
- **One channel** in the frames averaged in power (4096 bins of 3.9 kHz):
  - **Pilot:** 8-VSB's pilot is 11.3 dB below its data. Summed over its bins it stands 20.1 dB
    over a 3.9 kHz bin of a clean signal, whatever the FFT window. It is looked for within
    ±50 kHz of 309.44 kHz above the lower edge, and counts when it stands 10 dB over the bins
    around it.
  - **Plateau:** the median bin of the channel's middle (0.75 MHz in from each edge).
  - **Floor:** the deepest bin within ±40 kHz of either edge. Both neighbours' spectra end there
    (8-VSB's rolloff at the edge, ATSC 3.0 84 kHz short of it). The plateau over it is the
    carrier to noise, up to the transmitters' shoulders. It is read in one window at one gain,
    so the AGC's choices do not enter it.
  - **Kind:** `8vsb` when the pilot is found; `no_pilot` when the plateau stands 6 dB over the
    floor without one (ATSC 3.0, or another wideband signal); otherwise `vacant`.

### The TV scan (`services::atsc`)

- **When:** in ATSC mode only, one at a time; the gain is the AGC (slow attack) or a manual one.
- **Per channel:**
  - kind;
  - the pilot's frequency, its offset from the plan and its level over the plateau;
  - carrier to noise;
  - power (about dBm: the spectrum's scale taken back to 60 dB of gain);
  - the gain;
  - the window's clipping: full-scale samples a million, from the radio core's clip and sample
    counters.
- **Spectra:** each window's averaged spectrum is kept. `GET /api/v1/atsc/scan/channel/{n}` gives a
  channel and 0.5 MHz either side.
- **API and log:** `/api/v1/atsc/scan` (`GET`, `POST`, `cancel`, `options`). `/ws/live` pushes
  `atsc` while a scan runs, and the event log gets each scan's start and summary.

### UI

- **Header:** a mode selector. Switching to ATSC TV asks first, because the scanner stops
  following calls until it is switched back.
- **Tabs:**
  - scanner mode: Now, Activity and Systems;
  - ATSC mode: ATSC;
  - both modes: Diagnostics and Settings.
- **ATSC page:**
  - bands (VHF low, VHF high, UHF), frames per window and gain;
  - the scan's progress;
  - a table of every channel read;
  - the spectrum of the channel picked in the table, with the channel shaded and the plan's
    pilot and the window's LO marked.
- **Overload badge:** shown from 1,000 clipped samples a million (see the results below).

### Tools

`tools/atsc_check.py` compares a unit with an HDHomeRun on the same air. It reads the
HDHomeRun's lineup with its tuning (`lineup.json?show=all&tuning`: each program's RF frequency,
modulation and signal), runs a TV scan in ATSC mode, and puts the unit back in the mode it was
in. The comparison goes RF channel by RF channel, into `runs/atsc/<time>/`. Exit 1 when a channel
the HDHomeRun receives now is not found as its modulation says.

## Results on unit A (2026-10-03)

- **Antennas:**
  - Unit A: a UHF TV omni.
  - The HDHomeRun FLEX 4K (`10.0.0.117`): a VHF/UHF directional aimed at Jacksonville. It lists
    24 RF channels, 19 with a signal now.
- **Speed:** 33 channels in 15.3 s with the AGC.
- **Against the HDHomeRun:**
  - **Agreement:** 18 of the 19 channels it receives now were found with the right kind,
    including RF 18, Jacksonville's ATSC 3.0 host (WJXT, WJCT, WCWJ), as `no_pilot`.
  - **Pilots:** within 0.1 kHz of the plan, except RF 34 (-0.9 kHz) and RF 36 (-0.2 kHz).
  - **The miss:** VHF RF 11 (W11DV-D), below.
- **Extra stations:** the unit finds weak 8-VSB on RF 25, 29 and 32 (carrier to noise 3-13 dB)
  that the HDHomeRun does not list. The omni hears directions its directional antenna does not.
  Their pilots stand 10-22 dB over their plateaus, as a real pilot does.
- **The AGC is the right default.** Manual gain lowers the carrier to noise at UHF (RF 23: 15.0
  dB with the AGC at 61 dB, 10.5 at 40 dB, 4.8 at 30 dB).
- **Clipping:** the AGC left 0-2 full-scale samples a million in UHF windows and 18-210 in VHF
  ones. At 50 and 40 dB of manual gain nothing clipped (RF 4 at 50 dB: 73), and the carrier to
  noise stayed within 2 dB of the AGC's reads. A per-sample flag marked half the table; the
  badge now starts at 1,000 a million, above every rate seen.
- **VHF is weak on unit A.** The floor is about 13 dB above UHF's and the VHF stations read 8-11
  dB of carrier to noise; the HDHomeRun has them at 84-100 % strength. A UHF omni hears little
  VHF, and VHF windows take in images of strong UHF signals:
  - **RF 11** lies under a comb from 197.5 to 200.8 MHz in its paired window (LO 204.5 MHz).
    Read alone (LO 201.5 MHz) the comb is gone, and W11DV-D shows: `8vsb`, carrier to noise 3.3
    dB, pilot 11.6 dB. The comb moves with the LO. Its edge, 3.7 MHz below the LO, lands
    through the LO's third harmonic (613.5 MHz, spectrum inverted) at 617.2 MHz: the bottom of
    the 600 MHz downlink band. The AD9361 has no filter in front of it; a VHF filter or a VHF
    antenna is the fix.
  - **RF 4:** a comb of spurs 0.5 MHz apart (70.48, 70.98 MHz, ...) about 7 dB over a raised
    floor. It sits on the 6 dB occupancy line: `vacant` with the AGC, `no_pilot` at 40 dB.
    RF 13's image would put a pilot at 69.69 MHz and there is none, so it is not that; the
    cause is not found.
- **RF 14 (WFOX)** reaches unit A as a hump about 25 dB down at the channel's edges, so its
  pilot stands -7.7 to +1.8 dB over the plateau. With the AGC it is found (the pilot clears the
  bins around it); at 30 dB it reads `no_pilot`. The window does not cause it: RF 20 sits at
  the same offsets in its window and is flat.
- **Spurs:** CW lines at 480.0 MHz (12 × the 40 MHz reference) and 540.0 MHz in channel
  plateaus; none falls on a pilot.
- **Boot:** a restart in ATSC mode came up in ATSC mode with Clay County held for later.
  Switching back made it live.

## Checks

- **Host tests:** 445, among them:
  - synthetic spectra: an offset pilot, ATSC 3.0 beside 8-VSB, a vacant channel between strong
    ones, a station below the noise;
  - a TV scan on a fake radio (kinds, offsets, clipping rate, a channel's spectrum);
  - stopping a scan by leaving ATSC mode;
  - the mode round trip with the live site.
- **ARM check:** clean.
- **UI:**
  - `node --check` of every changed module;
  - the host build in headless Chrome: mode switch, redirects, a scan that fails cleanly;
  - unit A's page in headless Chrome: 33 rows, a channel's spectrum drawn.
- **Unit A:** builds `2026-10-03-atsc1` and `atsc2` were hand-deployed for the test. A is back on
  `2026-10-03-radio-core-fir1` in scanner mode on Clay County.

## Next

- **Names:** the stations' names and programmes from the PSIP. Decode a short raw IQ snapshot
  (the radio core's capture ring) in software: 8-VSB, trellis, Reed-Solomon, then the
  transport stream's VCT and EIT.
- **VHF:** a filter or a VHF antenna on the unit; in software, VHF channels could be read at two
  LOs and the cleaner read kept.
- **Live video:** a WASM decoder in the browser (MPEG-2 video, AC-3). It needs the transport
  stream in real time, which needs an 8-VSB demodulator in the gateware.
