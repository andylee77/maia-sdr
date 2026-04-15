# 043 — Fishball LVDS 1R1T bitstream + AD9361 temp-sense offset fix

## Summary

Two related Fishball Z7020 configuration fixes that together finish the "Board A
has only one antenna" cleanup:

1. **HDL bitstream switched from LVDS 2R2T to LVDS 1R1T** on all fishball builds
   (both `fishball7020_iio` and `fishball7020_p25`, since both source the shared
   `pluto/system_bd.tcl`). RX2 and TX2 are not antenna-connected on Fishball, so
   2R2T was burning half of the LVDS bus slots on samples nobody read. 1R1T
   doubles the per-channel sample-rate ceiling at the same `DATA_CLK`.

2. **AD9361 temperature-sensor offset rewritten** in both Tezuka device-tree
   variants. The previous `adi,temp-sense-offset-signed = <0xB6>` miscalibrated
   the sensor by ~26 °C — the board reported ~8 °C while stock firmware on the
   same silicon reported ~34 °C. New value is a placeholder (`<0xD0>`) that
   needs per-unit calibration against an external thermometer once Board A is
   back up.

The two changes are shipped together because the HDL change and the DTS
`adi,2rx-2tx-mode-enable` removal must be flashed as a matched pair — mismatched
framing between the `axi_ad9361` deserializer and the chip's SPI mode produces
garbage IQ samples, not silent fallback.

## Motivation

### Why 1R1T

From the `iio_info` dump of the second unit (running libiio + SDRTrunk at 4
MSPS), mode was confirmed `adi,2rx-2tx-mode-enable = 1`, both RX1 and RX2 were
live on the driver, and RX2 RSSI sat at 119.5 dB vs RX1's ~98 dB — classic
"nothing connected to the second port" signature. On Board A (Maia P25 target
running at 8 MSPS / 4 MHz `rf_bandwidth`) the same HDL flags apply because both
projects source the same `pluto/system_bd.tcl`.

On the LVDS bus, 2R2T multiplexes `{I1, Q1, I2, Q2}` across 4 consecutive
`DATA_CLK` periods; the `util_ad9361_divclk` divides `l_clk / 4` to produce a
`sampling_clk` that ticks once per complete IQ set. In 1R1T the bus only carries
`{I, Q}`, needs 2 `DATA_CLK` periods, and the divider automatically switches to
`l_clk / 2` via `adc_r1_mode` driving `util_reduced_logic`.

Net effect at the same `DATA_CLK`:

| Mode | `fs_complex` per channel | Ratio vs 2R2T |
|---|---|---|
| 2R2T | `DATA_CLK / 4` | 1× |
| 1R1T | `DATA_CLK / 2` | **2×** |

This gives the P25 pipeline 2× more oversampling headroom without touching
the custom DDC. It's the highest-leverage "bitstream only" improvement available
for the Maia DDC stage-1 filter weakness documented in
`project_p25_ddc_stage1_filter_weak`.

### Why the temp offset fix

`adi,temp-sense-offset-signed` is a per-chip signed-8-bit calibration knob the
AD9361 IIO driver applies when converting the chip's temperature register to
the `temp0 input` reading (in millidegrees C). The previous value of `0xB6`
(182 unsigned / −74 signed) was either a typo or a copy-paste from a different
unit's calibration, because on two independent boards running the Tezuka build
it produced physically impossible readings around 7-9 °C when ambient was
~25 °C and the AD9361 normally self-heats 10-15 °C above ambient.

The AD9361 temp sensor has a slope of ~1.14 codes/°C, so the ~26 °C error
corresponds to ~26 offset codes. `0xB6 + 0x1A = 0xD0` (208 unsigned / −48
signed) is the calculated starting point. Final per-unit calibration must be
done against a real thermometer once boards are available.

Note: **this is purely a reporting issue, not a thermal-control issue.** The
AD9361's internal thermal-compensation loops use the raw register value, not
the driver's post-offset reading, so RX sensitivity, LO drift, QEC, and
calibration are all unaffected by the bad offset. But the reading *is* used by
`p25-httpd`'s Board Info panel, so fixing it closes one "looks broken even
though it isn't" source of confusion.

## Changes

### HDL — `maia-hdl/projects/pluto/system_bd.tcl`

Around line 505, inside the existing `if {[info exists fishball]}` block:

```diff
 if { [info exists fishball]} {
 ad_ip_parameter axi_ad9361 CONFIG.CMOS_OR_LVDS_N 0
-ad_ip_parameter axi_ad9361 CONFIG.MODE_1R1T 0
+# LVDS 1R1T: RX2 is not antenna-connected on Fishball, so 2R2T wastes half
+# the LVDS bus. 1R1T doubles the per-channel sample-rate ceiling at the same
+# DATA_CLK. util_ad9361_divclk auto-switches /4 -> /2 via adc_r1_mode.
+# The Tezuka DTS must also drop `adi,2rx-2tx-mode-enable` to match.
+ad_ip_parameter axi_ad9361 CONFIG.MODE_1R1T 1
 ad_ip_parameter axi_ad9361 CONFIG.ADC_INIT_DELAY 30
 } else {
```

No other HDL file touched. In particular:

- `util_ad9361_divclk` stays at `SEL_0_DIV=4, SEL_1_DIV=2`. The selector is
  driven by `adc_r1_mode` at runtime, so the mux lands on `SEL_1_DIV=2` once
  the driver is in 1R1T.
- `util_wfifo` / `util_rfifo` (`util_ad9361_adc_fifo` / `axi_ad9361_dac_fifo`)
  stay at `NUM_OF_CHANNELS = 4`. They tolerate unused channels — the write
  side simply never sees valid strobes on channels 2/3, and the read side
  returns stale/undefined data on those lanes which nothing downstream reads.
- `util_cpack2` / `util_upack2` (`util_ad9361_adc_pack` /
  `util_ad9361_dac_upack`) stay at `NUM_OF_CHANNELS = 4`. These honor
  per-channel `enable_*` strobes dynamically, so channels 2/3 are runtime-gated
  off in 1R1T. The DMA path still writes 64-bit words to DDR, with the upper
  32 bits carrying zero — half the DMA throughput goes to zero bytes, but the
  IQ capture path still produces valid samples on channels 0/1.
- `adc_i_slice` / `adc_q_slice` still read from `dout_data_0` / `dout_data_1`
  of the wfifo (I0 / Q0). Unchanged.

This minimizes the blast radius of the HDL change and keeps the diff tiny. If
a later optimization wants to reclaim the half-empty DMA bandwidth, that is a
separate cleanup: change both `NUM_OF_CHANNELS` to 2 and rewire the cpack/upack
connections.

### DTS — Tezuka firmware

Two DTS files lose the `adi,2rx-2tx-mode-enable` property so the AD9361 IIO
driver asks the chip for 1R1T via SPI, matching the bitstream framing:

**`board/tezuka/fishball7020/dts/fishball.dts`** (Maia SDR variant):

```diff
 &adc0_ad9364 {
-	// This property is controlled by u-boot environment.
-	adi,2rx-2tx-mode-enable;
+	// LVDS 1R1T: matches axi_ad9361 CONFIG.MODE_1R1T=1 in the HDL bitstream.
+	// RX2 is not antenna-connected on Fishball and 2R2T wasted half the LVDS
+	// bus slots. `adi,2rx-2tx-mode-enable` deliberately NOT set here.
 };
```

**`board/tezuka/fishball7020/dts/fishball-p25.dts`** (P25 variant): same diff.

Two DTSI files get the new temp-sense offset placeholder with a big calibration
comment:

**`board/tezuka/fishball7020/dts/fishball.dtsi`** (around line 454):

```diff
 		/* AuxADC Temp Sense Control */

 		adi,temp-sense-measurement-interval-ms = <1000>;
-		adi,temp-sense-offset-signed = <0xB6>;
+		/*
+		 * FIXME: PLACEHOLDER CALIBRATION — verify against external thermometer.
+		 * Previous value 0xB6 (182u / -74s) reported ~8 C on a board that was
+		 * actually ~34 C (stock firmware matched ambient + self-heat). Each LSB
+		 * is roughly 1 C at the AD9361 slope of ~1.14 codes/C, so +26 C of
+		 * under-reporting maps to +26 codes => 0xB6 + 0x1A = 0xD0 as a starting
+		 * point. Calibrate per unit: boot, `iio_attr -c ad9361-phy temp0 input`,
+		 * compare to a real thermometer, adjust offset until matched.
+		 */
+		adi,temp-sense-offset-signed = <0xD0>;
 		adi,temp-sense-periodic-measurement-enable;
```

**`board/tezuka/fishball7020/dts/fishball-p25.dtsi`** (around line 513): same
diff.

## Post-boot validation procedure

Both changes need Board A available before they can be verified. The expected
sequence once Board A is back up:

### Step 1 — rebuild HDL + firmware

```bash
# In maia-sdr repo
./build_fpga.bat --p25    # Rebuild the fishball7020_p25 bitstream with MODE_1R1T=1
                          # Produces new XSA at maia-hdl/projects/fishball7020_p25/...

# In tezuka_fw repo
make fishball_p25_7020_defconfig
make                      # Buildroot picks up the new DTS from the P25 overlay
                          # and builds the firmware image
```

Flash the resulting image to Board A.

### Step 2 — confirm 1R1T is live

From Windows:

```bash
iio_attr -u ip:192.168.2.1 -D ad9361-phy adi,2rx-2tx-mode-enable
# Expected: attribute should not be present, or read "0"

iio_info -u ip:192.168.2.1 | grep -A2 "cf-ad9361-lpc" | head
# Expected: only voltage0 and voltage1 input channels (not voltage0..3)

iio_attr -u ip:192.168.2.1 ad9361-phy rx_path_rates
# Expected: RXSAMP can go up to ~15.36 MHz or ~30.72 MHz depending on how the
# driver chooses the BBPLL chain — confirm fs ceiling has moved up
```

### Step 3 — run the P25 pipeline and measure

Bring up `p25-httpd`, connect to the control channel at a known site, watch
the /api/system payload's `control_crc_rate`. Compare against the pre-change
baseline (currently ~80 % at rf_bandwidth=4, fs=8 MSPS). Expected change: CRC
rate holds or improves. Main benefit isn't immediate at the current 8 MSPS
config — the leverage comes from being *able* to go to 15.36 MSPS / 4 MHz
rf_bandwidth for real oversampling headroom. Try both.

### Step 4 — calibrate temperature offset per unit

For each physical Fishball board:

```bash
# Read current reading from the Tezuka build
iio_attr -u ip:<board-ip> -c ad9361-phy temp0 input
# Note the value in millidegrees C

# Stick a thermometer on the AD9361 package (or use a reliable board-case
# temp proxy + known self-heat offset). Note the actual temperature.

# Compute correction: each offset code is ~1 C.
# new_offset = current_offset + (actual_temp_C - reported_temp_C)
# Example: reported 12 C, actual 36 C, current offset 0xD0 -> 0xD0 + 24 = 0xE8

# Try it at runtime first (no reboot needed):
iio_attr -u ip:<board-ip> -D ad9361-phy \
    adi,temp-sense-offset-signed <computed-value>
iio_attr -u ip:<board-ip> -c ad9361-phy temp0 input

# Once happy, persist by editing fishball.dtsi / fishball-p25.dtsi and
# rebuilding the Tezuka image.
```

If multiple units have meaningfully different offsets, consider capturing a
per-serial calibration table in the Tezuka build rather than baking a single
value into the DTS.

## Risks and rollback

- **Data corruption risk if only one half of the pair ships**: if the HDL is
  rebuilt but the DTS is not updated (or vice versa), the deserializer framing
  and the AD9361 SPI mode mismatch, and capture produces garbage. Always ship
  the HDL bitstream and the firmware image together.
- **`util_cpack2` gating behavior**: relying on dynamic per-channel enables to
  suppress the empty 2R2T slots is the documented `util_cpack2` feature, but
  if the AD9361 driver in this kernel version happens not to drive the
  `adc_enable_i1`/`adc_enable_q1` signals low in 1R1T, channels 2/3 would
  capture stale data. Mitigation: confirm by reading the first few captured
  samples from the DMA path and checking channels 2/3 are all zero. If not, the
  fallback is to change `NUM_OF_CHANNELS 4 → 2` on the cpack/upack and rewire
  the connections — follow-up change.
- **Temp offset is a placeholder**: `0xD0` is a best-guess calculated from the
  observed error. It will not be exactly right on any given unit. Cosmetic only
  — does not affect RF behavior — but do not treat `temp0 input` as accurate
  until Step 4 has been run for the specific board.
- **Rollback**: revert both files in each repo; no schema or API changes.

## Related memory

- `project_antenna_swap_stopped_working.md` — RX2 unused context
- `project_p25_ddc_stage1_filter_weak.md` — the reason oversampling headroom
  matters
- `feedback_bandwidth_sweep_8mhz.md` — why widening rf_bandwidth collapsed CRC
- `doc/ADI_HDL_INTEGRATION.md` §5.3, §5.5, §5.10 — the LVDS framing,
  `sampling_clk` derivation, and bandwidth-math sections this change rests on
