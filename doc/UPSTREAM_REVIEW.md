# Upstream review, 2026-11-11

What Maia SDR, Tezuka and their forks changed since this fork left them, what other P25 and DMR
FPGA work exists for the Zynq-7020 class, and what to do about it. It follows the first review
(`doc/changes/002_upstream_sync_2026-04-07.md`).

**How it was done.** Both upstream remotes were fetched, and the original Maia repository
(`maia-sdr/maia-sdr`) was fetched into `refs/remotes/maia/*` with its tags under
`refs/tags/maia-upstream/*`. Upstream code was read with `git log`, `git diff` and `git show`.
Merges were tried in memory with `git merge-tree --write-tree`; nothing was checked out, merged or
built, and no unit was touched. Forks, pull requests and issues were read through the GitHub API.
Every fact has its commit or link; what could not be verified is in "Not verified" at the end.

## The short version

1. **A kernel image check is due on unit A.** Tezuka found that a self-extracting `zImage`
   corrupts the kernel's read-only data at boot once the unpacked kernel passes about 14.5 MB
   ([issue 450](https://github.com/F5OEO/tezuka_fw/issues/450)). This fork's SD image boots a
   self-extracting `zImage`, and its kernel unpacks to 15.69 MB. Upstream saw the fault on the
   flash boot path, which this fork does not use, so the exposure is not proven either way.
2. **Maia's DDC mixer wraps on strong input, and the lanes use it.** An open pull request to
   F5OEO's fork fixes it with one more bit
   ([PR 18](https://github.com/F5OEO/maia-sdr/pull/18)). It belongs in the next bake.
3. **F5OEO's `main` is frozen; all its gateware work is on `refactor`**, which removes the
   `projects/pluto` design both fork projects are built on. A merge is textually clean and breaks
   the build. The fork's Vivado projects should stop depending on upstream's project tree.
4. **Tezuka moved a long way** (489 commits: kernel 6.12, Buildroot 2026.05, a new U-Boot, a new
   image layout). A merge conflicts in 14 files and needs new device trees. Nothing in it is
   needed by the scanner today; the `maia-kmod` patches are worth porting now, and the
   first-boot flash format and chrony later.
5. **Original Maia changed nothing in the gateware** since v0.10.0.
6. **No open P25 receiver for an FPGA exists, and no open P25 or DMR demodulator in HDL.** Two
   projects from October 2026 receive DMR on Pluto-class boards with a channelizer in the PL and
   the demodulators in software, the split 079 chose: `earshot` (a 512-bin filter bank on a
   Zynq-7010) and RadioFoundry Gateway (16 channels on a Fishball 7010; its HDL is not
   published). Among the forks, an airband receiver built into Maia's gateware uses
   time-multiplexed DDC lanes. All three bear on bake B.

## 1. The fork points

| Repository | Upstream | Left it at | Upstream now | Commits since |
|------------|----------|------------|--------------|--------------:|
| `maia-sdr`, `fishball-p25` (554 commits over `main`) | `F5OEO/maia-sdr` `main` | `c49db1f`, 2025-11-21 ("Use vivado 2023") | the same commit | 0 |
| | `F5OEO/maia-sdr` `refactor` | the same | `ba62a78`, 2026-11-03 | 110 (53 at the 2026-04-07 review) |
| | `maia-sdr/maia-sdr` `main`, F5OEO's own upstream | `01b36ba`, 2024-12-01 (v0.10.0 + 3), where F5OEO left it | `2afaba9`, 2026-11-03 (v0.12.1 + 1) | 40 |
| `tezuka_fw`, `fishball-dev` (64 commits) | `F5OEO/tezuka_fw` `main` | `8a6c6ec`, 2026-03-25 (tag `Tezuka_0.3.0`) | `49403b9`, 2026-11-05 (latest tag v0.3.24, 2026-10-17) | 489 (447 without merges) |
| `maia-hdl/adi-hdl` (submodule) | `analogdevicesinc/hdl` | `cf81ab1`, 2024-09-12, plus one local cherry-pick (`01ef629`) | release `2026_R1`, 2026-10-01 | not counted |

- **F5OEO never merged the original Maia back.** Its `main` carries 76 commits of its own over
  `01b36ba`.
- **F5OEO's other branches:** `gpio` is contained in `refactor`, `sweep` in `main`, `maia-iio`
  dates from 2024. Tezuka's `future` branch is contained in its `main`.
- **The submodule's commit exists only in the local clone.** `01ef629` is ADI's `92534dc1d`
  ("axi_ad9361_tx: Use incrementing cnt to improve timing margin") picked onto `cf81ab1`
  (`doc/ADI_HDL_INTEGRATION.md` records it). GitHub has no such commit, and ADI's `main` and
  `2026_R1` do not contain `92534dc1d` either (it sits on ADI's branch `fix_fmcomms2_timing`).
  A fresh clone of the fork cannot check the submodule out.

**Trial merges** (in memory, nothing checked out):

| Merge | Result |
|-------|--------|
| `fishball-p25` + F5OEO `refactor` | No textual conflict. The build breaks: `fishball7020_p25/system_bd.tcl` and `fishball7020_hwval/system_bd.tcl` both `source ../pluto/system_bd.tcl`, and `refactor` has no `projects/pluto` |
| `fishball-p25` + original Maia `main` | 10 conflicts, all in `maia-httpd` and `maia-wasm` (`Cargo.toml`, `Cargo.lock`, the changelogs, `waterfall.rs`, `websocket.rs`). They are F5OEO's edits against the original's, not the fork's: F5OEO's `main` and `refactor` conflict in the same 10 files |
| `fishball-dev` + Tezuka `main` | 14 conflicts: `.gitignore`, `Config.in`, `README.md`, `build.sh`, `S55hostkeys`, `S91nfs-mount`, `post-build.sh`, `post-image.sh`, the fishball7020 `fishball.dts`, `fishball.dtsi` and `plutomaia.its`, `fishball_maiasdr_7020_defconfig`, `maia-httpd.mk`, `maia-wasm.mk` |

## 2. What upstream changed

### 2.1 Original Maia SDR, v0.10.0 to v0.12.1

Releases: v0.11.0 (2025-04-12), v0.12.0 (2025-11-09), v0.12.1 (2026-11-03).

| Area | Change | The fork | Worth taking |
|------|--------|----------|--------------|
| Gateware (`maia_hdl`) | None. The only edit is the version string, 0.6.1 to 0.6.3 | Imports `axi`, `cdc`, `ddc`, `dma`, `fft`, `fifo`, `register`, `spectrum_integrator`, `pluto_platform` | Nothing to take |
| Gateware tests | The cocotb tests run under cocotb 2.0 (`c5059ed`, `1f10f55`, `a1c4a58`) | Its own cocotb bench is `scanner-hdl/test_cocotb/` | Only when the venv moves to cocotb 2 |
| `maia-kmod` | Builds on kernels 6.4+ and 6.11+ (`6f997c6`): `class_create`, `ida_alloc_range`, the `remove` signature | Unchanged; the firmware pulls tag v0.10.0, whose driver source is identical to v0.12.0's | With a kernel move, not before |
| `maia-httpd`, `maia-wasm`, `maia-json`, `maia-pac` | Rust edition 2024, dependency updates, versions in the REST API (`7bfd740`) | Not changed, and not in the scanner's image (the P25 defconfig selects the scanner, its bitstream and `maia-kmod`) | No |
| Branches | `cors` (one commit, 2025), `fosphor` (2023) | | No |

### 2.2 F5OEO's `refactor` branch

This is where F5OEO's gateware lives now: Tezuka's bitstreams have been built from it since
2026-03-26 (`tezuka_fw` `6703f12`). 235 files changed, 26,921 lines added.

| Area | Change | The fork | Worth taking |
|------|--------|----------|--------------|
| Project layout | `projects/<board>` gives way to `projects/boards/<board>` (ports, PS7, constraints, top), `projects/common/*.tcl` and one `projects/tezuka` project built per board. `pluto`, `plutoplus` and every `*_iio` project are gone | Both fork projects source `../pluto/system_bd.tcl` | Not as a merge. See R3 |
| Speed grade | fishball7020 is built as `xc7z020clg400-2`; the commit calls it "a hack for timing" and "not true" (`108aa43`) | Builds `-1` and meets timing with no waiver | No. Do not adopt |
| Vivado | 2025.2 (`sourceme2025.first`). A hook reruns `route_design` before post-route optimisation, for hold violations the 2025 router leaves (`6ab1231`, `projects/tezuka/hold_fix.tcl`). FSBL creation left the HDL build (`bd04d36`) | Vivado 2023.2 | The hook, if the fork ever moves to 2025 |
| `maia_hdl` core | `fft.py`: only its Verilog generator script. `spectrometer.py` and `maia_sdr.py`: a raw FFT output, marked "not yet working properly" (`8ce939c`). The DDC, DMA, FIFO, CDC, AXI and register modules are untouched | The imported classes are unchanged | Nothing to take |
| New blocks (Amaranth) | `symfront`: a DVB-S2 symbol front end for fishball7020 (mixer, Gardner timing loop, AGC; MIT; a numpy model it matches bit for bit, with tests; `ba62a78`). `iqburst` (`09dcd3d`). `modulator` (`e962e44`). `dvbs2rx`, "first attempt" (`e7fb30d`) | None | `symfront` as a worked example of a timing loop in the PL, built the way 085 built the 8-VSB block |
| DVB-S2 transmit | `OpenResearchInstitute/dvb_fpga` with local patches | None | No |
| The fishball7020 board | I2C1 over EMIO to pads (`a859744`), quad SPI (`9851d0a`), a 16550 UART (`9c1cd64`), 64 EMIO GPIOs, a GPS-disciplined oscillator core from a 1 PPS input with a 25 MHz output (`boards/fishball7020/vcxo_ctrl.v`), sync in and out | The fork's top level and constraints are its own files | Only with the extension board these serve |
| Stream formats, sweeper | 8, 12 and 16-bit IIO packing, sync frames, a frequency sweeper | None | No |
| New boards | E310, Nano, SignalSDR Pro, PlutoSky R2, PCIe SDR 7010, OpenSDRLab 7010 mini | | No |

Ten pull requests are open against `refactor`, all from one contributor (see 3.1). Two matter
here: the mixer fix and a raw complex FFT capture.

### 2.3 Tezuka

Releases since the fork point: 35 tags, from 0.3.0 (2026-04-03) to v0.3.24 (2026-10-17).

| Area | Upstream change | The fork | Worth taking |
|------|-----------------|----------|--------------|
| Kernel | ADI 6.1 to ADI 6.12 (`358fc6e`; now `bf1d49f`, 2026-07-23). In the AD9361 driver between the two pins: a calibration error is returned (ADI `a7974e151`), a debugfs race is fixed (ADI `f4d1d12c2`), the rest is housekeeping. `dma-axi-dmac` gained scatter-gather and cyclic fixes | Pins ADI `04fbdbb` (2024-09-04). Its device trees would need the 6.12 style: MDIO subnodes (`a4d2039`), `interrupt-parent` on the DMA nodes (`1f0454d`), a device-tree directory instead of a file list | Watch. A change of its own, with no gain for the scanner yet |
| `maia-kmod` | v0.12.0 with four patches in `package/maia-kmod/`: kernel 6.4 compatibility; the mapping's address cleared on close and the cache-invalidate ioctl refused without a mapping (`07b22d9`); mmap, close and invalidate serialised (patches 0003 and 0004; 0004's own text says it was not yet tested on hardware) | v0.10.0, no patches. The scanner maps three rxbuffer devices and invalidates per sub-buffer (`scanner/src/hardware/mmio.rs`) | Yes, patches 0002 to 0004. Low exposure (one process maps each ring once), low cost |
| Boot image | The kernel goes into the image as `Image.lzma`, unpacked by U-Boot (`d99264f` for fishball7020), with a build guard (`44cdd7a`). The ramdisk is xz. The post-image script is split in three. U-Boot loads the bitstream from the card instead of from `BOOT.bin` (`7544da2`) | Its own `post-image.sh` (a conflict). The SD `uImage` is a `zImage`, compression "none" | Check first (R1) |
| U-Boot | Xilinx `u-boot-xlnx` `a09d266` (the 2025.01 rebase), one config with per-board fragments, a boot menu, a USB serial console, USB mass storage, DFU fallback | ADI `u-boot-xlnx` `90401ce`, its own environment and U-Boot device tree | Watch |
| Buildroot | 2025.05 to 2026.05.2 (`buildroot.version`), Bootlin gcc 15.1 in place of Arm's toolchain, 64-bit `time_t`, tarball downloads, ccache, a CI matrix | `getbuildroot.sh` fetches 2025.05. `build.sh` exists on both sides (a conflict) | Watch |
| fishball7020 | Its own device trees (`ef91d4e`), the 7020's own FSBL (`74d8695`), an underclocked FSBL "for fishball bad DRAM version", CPU 495 MHz and DDR 396 MHz (`1b9e8c9`), an OLED on I2C1 (`f4391cf`), bitstreams from `refactor` | Added its own fishball7020 device trees and `.its` (conflicts) and fixed the FSBL mismatch itself (`tezuka_fw` `doc/changes/002`) | Note the DRAM variant (open question 4) |
| Time | chrony as the only time daemon, serving NTP once it is synced (`e992d6b`); kernel PPS clients (`ab70249`) | The P25 defconfig selects no time daemon, and busybox's `ntpd` is off | Yes, when the unit's clock is taken up (089's open item) |
| System | A blank flash partition formatted on first boot (`cc0f7d0`, `S20jffs2`). `sysctl` settings for `iiod` streaming: dirty pages allowed up to 80 % of memory, 16 MB socket buffers, no real-time throttling (`70acf3d`). The static address restored after a link loss (`49403b9`) | A blank partition needs `device_format_jffs2` by hand (`S22serial`). No `sysctl.conf`. Its `ifplugd.action` predates the fault that `49403b9` fixes | `S20jffs2` is a small port, useful for a new unit. The `sysctl` values suit a streamer, not a recorder writing to a card: no |
| Applications | `zynqrx`, a DVB-S and DVB-S2 receiver for the A9 (GPL-3.0). A browser player on mpegts.js with a transport stream analyser. A signal classifier that matches spectrum shapes against templates (`2e682fe`, `4f4eb82`). kalibrate, DATV, SatDump, longmynd, WireGuard, an MQTT dashboard | None | Reference only (section 4) |

### 2.4 The build chain, side by side

| Item | The fork | Upstream now |
|------|----------|--------------|
| Vivado | 2023.2 | Original Maia 2023.2; F5OEO `refactor` 2025.2 |
| Amaranth | `>= 0.5, < 0.6` | The same in all three |
| ADI HDL | `cf81ab1` + one local pick | F5OEO `cf81ab1`; original Maia `065c8f1`; ADI's `2026_R1` targets Vivado 2025.1 |
| Kernel | ADI 6.1 (`04fbdbb`) | ADI 6.12 (`bf1d49f`) |
| U-Boot | ADI `90401ce` | Xilinx `a09d266` (2025.01) |
| Buildroot | 2025.05 | 2026.05.2 |
| Toolchain | Arm GNU (external) | Bootlin gcc 15.1 |
| `maia-kmod` | v0.10.0 | v0.12.0 + 4 patches |
| Kernel in the SD image | `zImage` in a `uImage`, unpacks itself | `Image.lzma`, unpacked by U-Boot |

## 3. Other forks and branches

The original Maia has 81 forks, F5OEO's Maia 18 and Tezuka 108. Checked, for each fork pushed
since the fork points: every branch's name, and for its first 25 branches the distance from the
parent and the last commit (two Tezuka forks have more branches than that). **Apart from this
project's own forks, no branch name or last commit mentions P25, DMR, trunking, TETRA or NXDN.**

### 3.1 Maia forks

| Fork and branch | What it is | For this project |
|-----------------|-----------|------------------|
| `mbehn1976/maia-sdr`, eleven open pull requests to F5OEO (ten to `refactor`, one to `main`; 2026-10) | **[PR 18](https://github.com/F5OEO/maia-sdr/pull/18): the DDC mixer keeps 13 bits.** Also the DDC's output to IIO rounded instead of truncated ([PR 17](https://github.com/F5OEO/maia-sdr/pull/17)), I and Q crossing clock domains in one FIFO ([PR 15](https://github.com/F5OEO/maia-sdr/pull/15)), and a raw complex FFT capture with an I/Q waterfall ([PR 12](https://github.com/F5OEO/maia-sdr/pull/12)). Each comes with a hardware test and its results | **PR 18 applies to the lanes** (below). PR 12 taps the FFT before the power stage, which is what 079's item 10 (a classifier tap) needs |
| `juchong/maia-sdr`, `pluto-airband` (26 commits, 2026-06 and 07) | A multichannel AM airband receiver built into Maia's gateware for Pluto and Pluto+ (Zynq-7010). Section 4 | **The closest relative to bake B** |
| `F4HSL-Yves/maia-sdr`, `dvbs2-symfront` | The `symfront` work before it was merged, with a decision-directed carrier stage that was reverted | Reference |
| `gretel/maia-sdr`, `refactor-rebased-2026-05-02` | F5OEO's `refactor` rebased onto the original Maia, and a fix for `test_packer` under numpy 2 (`c67f7c5f`: a `uint8` shift that overflows) | `test_packer` is one of the three upstream tests CLAUDE.md lists as failing with numpy 2. The other two are not fixed there |
| `JohnG210/maia-sdr-snail`, `sweep` (27 commits, 2026-03) | Replaces the spectrometer, DDC and recorder on a Zynq-7010 with a pulse engine: pulse descriptors (time of arrival, an FFT frequency estimate), an event table and transmit playback | Its receive half is the shape of 079's item 9 (a burst detector in the PL). The transmit half is outside this project |
| `mesteruvd/maia-sdr`, `feature/lora-candidate-detector` | LoRa candidates found and classified by bandwidth in `maia-httpd`, from spectrometer frames | An idea for data mode's classifier, in software |
| `r4d10n/maia-sdr` `SNR-meter`; `sq6emm` `datv-ddc`; `ve3ukw`, `sv1eia`, `cvalentine99` (reference clocks for PlutoSky R2 and SignalSDR Pro); `pa3gsb` (LibreSDR transmit); `Dmytro-K` | Board and transmit work, a cursor SNR meter | None |

**PR 18 and the lanes.** Maia's mixer rotates a 12-bit sample and keeps 12 bits
(`maia-hdl/maia_hdl/mixer.py`; its docstring says the output overflows above an amplitude of
2047, and `clamp_nbits` in `util.py` is a modular wrap, not a clamp). A rotated component can be
as large as the sample's magnitude, which reaches 2,896 when I and Q are both near full scale.
The pull request's author measured, on a Pluto+ with a full-scale test tone, 246,429 wrapped
samples in 15.36 million, each spread by the decimating FIR into an error of about 200 LSB at the
DDC's output, and none with 13 bits. The fork's lanes are `maia_hdl.ddc.DDC` with a 12-bit input
(`scanner-hdl/radio_core/p25ddc.py`, `p25_top.py`), so they wrap the same way. By reasoning, not
measurement: for a single carrier or for noise-like input the wrap sets in at about the level
where the ADC itself would clip, so it follows the lane header's `adc_clips`; the difference is
that a clip compresses and a wrap flips the sign. Samples with both components large wrap
without either rail clipping.

### 3.2 Tezuka forks

| Fork and branch | What it is | For this project |
|-----------------|-----------|------------------|
| `gretel/miyazaki` | Where upstream's kernel 6.12 and build work came from. `fix/maia-kmod-crash` (`baada05c`) is another fix for the rxbuffer races, with a use-after-free on module unload and range guards. Also irqbalance and LTO trials for fishball7020 | A second opinion on the `maia-kmod` patches |
| `mbehn1976/tezuka_fw`, `dev/next` (116 commits ahead) | The classifier's features, the dashboard's spectrum, the raw capture's integration, a note on the AD9363's extended tuning range | The classifier (section 4) |
| `F4HSL-Yves/tezuka_fw` | `zynqrx` development: the FPGA front end, impulse erasure, an LDPC budget | Reference |
| `MUSTARDTIGERFPV/tezuka_fw`, `gbe-streaming-throughput` | Measured limits of IIO streaming over gigabit Ethernet. Its cacheable-DMA kernel patch turned out to be a regression (`e621aa00`). IRQ pinning, jumbo frames | Only if raw IQ is to leave the unit over Ethernet |
| `mavpii/tezuka_fw`, `optimized` | Stops the dashboard stack and pins the SDR threads at boot | An idea: pinning the scanner's threads |
| `muhiddinov/tezuka_fw`, `arm-optimized` | An FSK modem on the PS (liquid-dsp, NEON, a SIGILL fix for the A9) | Data mode reference |
| `cvalentine99/tezuka_fw` | The SD boot partition checked when dirty and mounted read-only; classifier tooling | The fork already runs `fsck.fat` before mounting (`S60scanner`) |
| `RichardPar`, `Dmytro-K`, `sq6emm` (a radiometer), `electro-logic`, `andrethomas`, `dukenukke`, `ve3ukw`, `sv1eia` | Board fixes, dashboards, reference clocks | None |

## 4. Other P25 and DMR FPGA projects

The fork's own split, for comparison (079, `scanner/doc/DESIGN.md`): the PL holds DDC lanes at
50 kSPS, a spectrometer, a capture ring and the 8-VSB block; the PS holds every P25 and DMR
demodulator, the framing, the error correction, trunking and the vocoder.

### 4.1 Relatives among the forks

| Project | In the PL | On the PS | State and licence | To learn or reuse |
|---------|-----------|-----------|-------------------|-------------------|
| `juchong/maia-sdr`, `pluto-airband`: an AM airband receiver inside Maia | The 12-bit IQ, at up to 16 MSPS, goes to six time-multiplexed lanes. A lane serves three channels with one NCO, mixer and CIC, the channels' state in block RAM. A folded 63-tap FIR cleans up after the CIC. A CORDIC magnitude, a DC block and an audio CIC, folded the same way, demodulate. Records of sequence, channel and sample go to a cyclic DMA ring | `maia-httpd`: the channel plan over REST, framed audio over TCP, a watchdog for a stalled DMA | 26 commits, 2026-06-14 to 07-07. 18 channels on a Zynq-7010; 21 on seven lanes overflowed it (18,234 of 17,600 LUTs, `04854404`). Maia's licences (`maia-hdl` MIT); the airband files have no header of their own | **A lane design for bake B** in which a channel costs RAM, not DSPs (a DDC lane here costs 13 DSPs and 7 block RAM tiles, 079). Its lessons: one input every four clock cycles needs the PL clock at four times the sample rate, which took 65.3 MHz for 16 MSPS (`3fb2ced0`); a reserved region at the top of the DDR collided with the kernel's CMA and hung the boot (`e6b1a63c`); an approximated magnitude put tones in the audio until the CORDIC replaced it |
| F5OEO's `symfront` with Tezuka's `zynqrx`: DVB-S2 on fishball7020 | A mixer, a Gardner timing loop with a samples-per-symbol register (2.0 to 8.0) and an AGC; one symbol per sample out through the IIO DMA | The DVB-S and DVB-S2 receiver in C++: two decoder threads, an LDPC kernel in NEON; a browser player | Merged 2026-11-03 (`ba62a78`; `tezuka_fw` `ef53bdb`). `symfront` MIT, `zynqrx` GPL-3.0 | The same split as 085, for the same reason: `zynqrx`'s README measures about 2.6 CPU-seconds per Msym/s on the A9 and concludes that past 1 Msym/s the symbols must come from the FPGA. The method carries over; GPL-3.0 code does not |
| Tezuka's `classifier` | Nothing: it reads `maia-httpd`'s waterfall | Correlates each normalised spectrum with stored templates, gates on bandwidth, peak-to-average ratio, flatness and peak count, follows drift and duty cycle, and publishes the match over MQTT | `app/classifier/classifier.c`, 898 lines, 2026-08-26 to 10-01. The repository is GPL-3.0 | A first list of spectrum features for the survey ("identify everything on air"). It sees magnitude only, which is why its author asked for the complex FFT tap (PR 12) |
| `JohnG210/maia-sdr-snail`: a pulse engine | Pulses detected with a time of arrival and an FFT frequency estimate, written as descriptor records; an event table; playback | Not looked at | 27 commits, 2026-03. No licence beyond Maia's | Item 9's burst detector: what a detection record holds. Nothing else |

### 4.2 The survey

**No open P25 or DMR demodulator in HDL was found, for any FPGA.** A GitHub search of repository
names and descriptions finds nothing for `p25 verilog`, `p25 vhdl`, `c4fm fpga`, `c4fm verilog`,
`dmr vhdl`, `dmr zynq` or `apco25 fpga`; `p25 fpga` and `p25 zynq` find only this project's own
archived private repository, and `dmr fpga` only `earshot` below. No paper or thesis with a
public P25 or DMR receiver in logic turned up. What exists is two projects from October 2026 that
receive DMR on Pluto-class boards, both with a channelizer in the PL and the demodulators in
software, which is 079's split.

| Project | In the PL | In software | State and licence | To learn or reuse |
|---------|-----------|-------------|-------------------|-------------------|
| [`coryking/earshot`](https://github.com/coryking/earshot): DMR, NXDN and FM from a Pluto (Zynq-7010) | A 512-bin, 2x-oversampled polyphase filter bank on the 25.6 MSPS stream: 50 kHz bins at 100 kSPS each, a 4,096-tap prototype, Xilinx's FFT core, power and activity per bin. Two `axi_dmac` streams: a ring of every bin, and the bins picked for forwarding. The transmit path is removed to make room | A producer on the Pluto sends the picked bins over TCP. A Rust engine probes each busy channel for DMR, NXDN48 and FM. **P25 and trunking are listed as not done** | Created 2026-10-22, 135 commits. GPL-3.0-or-later. Verilog with a bit-accurate Python model. The whole design: 13,149 LUTs (74.7 % of a 7010), 74 DSPs, timing met at 100 MHz; the FIR takes 16 DSPs and 12 block RAMs, the FFT 2,304 LUTs and 22 DSPs (`fpga/README.md`) | **A measured filter bank at 079's own channelizer rate** (25.6 MSPS), for bake B's choice between DDC lanes and step 3b. Activity detection in the PL that decides which bins leave it. GPL: read it, do not copy it into the MIT gateware. Its AMBE+2 decoder is, by its README, derived from DVSI's code in a radio's firmware: not for reuse |
| [RadioFoundry Gateway](https://github.com/RadioFoundry/RadioFoundryGateway): a DMR repeater and monitor | "The FPGA channelizer", 16 channels, and a transmit interpolator beside ADI's AD9361 interface, for a **Fishball 7010** | `rdofradio` on the Pluto's ARM demodulates DMR on every channel | Created 2026-10-24, 360 commits, MIT. Only the bitstream is published (`tools/pluto/fpga/`); its README says it is built in a repository `rfg-pluto`, which is not on GitHub | A claim that 16 DMR channels demodulate on a Zynq-7010's two cores. Nothing to reuse in the PL without the HDL |

**Other narrowband waveforms in a Zynq PL:**

| Project | What is in the PL | State and licence | To learn or reuse |
|---------|-------------------|-------------------|-------------------|
| [`silencer79/tetra-zynq-phy`](https://github.com/silencer79/tetra-zynq-phy): a TETRA base station on LibreSDR (Zynq-7020, AD9363) | The sample-rate path in Verilog: a pi/4-DQPSK demodulator, timing recovery, sync detection, a rate-1/4 Viterbi decoder, burst capture (`rtl/rx/`). Signalling and the voice codec are on the ARM | 458 commits, 2026-04 to 2026-10. Its README gives the RTL as MIT (no licence file) and calls the project an AI-assisted experiment, with registration and a group call verified against a Motorola handset | The only open timing loop, sync detector and Viterbi found for a land-mobile waveform on this exact hardware. A reference if a demodulator ever moves to the PL; 079 keeps them in software |
| [`OpenResearchInstitute/pluto_msk`](https://github.com/OpenResearchInstitute/pluto_msk): an MSK modem on a Pluto | Modulator, demodulator, NCO, loop filters, a PRBS generator and checker, in VHDL as an out-of-tree ADI HDL project | CERN-OHL-W-2.0, last push 2026-07 | A bit-error test built into the PL |
| [`M17-Project/OpenHT-fpga`](https://github.com/M17-Project/OpenHT-fpga): M17 on a Lattice FPGA | FM, AM and SSB demodulators, a CORDIC, a rational resampler, in VHDL | GPL-3.0, last push 2024-06. Not a Zynq | Little |

**Other standalone receivers on this class of board**, for their split: DeimosSDR (802.11 receive
entirely in a Pluto's fabric, MIT, 2026), `web888-ng-hw` (13 DDCs or a 30 kHz-bin channelizer on a
Zynq-7010, GPL-3.0, 2026), Pavel Demin's `red-pitaya-notes` (eight DDCs in the PL feeding FT8
decoders on the ARM, MIT), openwifi (the whole 802.11 PHY in the PL of a Zynq-7020 with an AD9361,
AGPL-3.0). None carries anything for P25 or DMR.

**Cores:**

- **Error correction:** `dshekhalev/FEC` (SystemVerilog, MIT: Reed-Solomon, BCH, Golay, Viterbi)
  and `russdill/bch_verilog`. Neither says it covers P25's codes over GF(64). The fork decodes in
  software, so nothing is needed.
- **ATSC:** two open 8-VSB receivers in logic. `Xilinx/RFNoC-HLS-ATSC-RX` (2017, Vivado HLS for
  USRPs, no licence) and `cfbird/atsc-fpga-tier3` (2026-09, four commits, MIT, a 24-tap LMS
  equalizer on a Spartan-6; its README claims live reception). Neither was read further.
- **Vocoder:** none in HDL. In Rust, `openBLIP25/blip25-vocoder` (MIT, 2026-08, with a patent
  notice) covers the P25 full-rate and half-rate codecs: a second reference for
  `scanner/src/audio/codec`, half rate included.

**PC software checked for an FPGA or on-board variant:** SDRTrunk, OP25, trunk-recorder, DSD-FME
and dsd-neo have none, and no report was found of any of them running on a Pluto's or a Zynq's
own processor.

## 5. Recommendations

### Do now

- **R1. Check unit A's running kernel against the build's `Image`.** The facts:
  - upstream's fault: with a self-extracting `zImage`, about 370 KB of the kernel's read-only
    data is corrupted on every boot from physical address `0xE90000` (image offset `0xE88000`)
    once the kernel is larger than that; the mechanism inside the `zImage` stub was not found
    ([the analysis](https://github.com/F5OEO/tezuka_fw/issues/441#issuecomment-5622988578));
  - upstream's case: a flash boot, the `zImage` loaded at `0x8000`;
  - this fork: `post-image.sh` wraps `zImage` in a `uImage` with compression "none", load
    address `0x2080000`. The image built on 2026-11-10 (`tezuka_fw/output_images/uImage`)
    unpacks to 16,447,804 bytes, 1.2 MB past that offset.

  The check is read-only: compare the unit's memory from `0xE90000` with the `Image` from
  `0xE88000`, over the kernel's read-only sections. If it differs, take upstream's fix: an
  `Image.lzma` that U-Boot unpacks (`d99264f`), which needs LZMA and a large enough `bootm`
  length in U-Boot. An unpacked `Image` in the `uImage` would avoid the stub without either;
  that is a suggestion, not tested. **Cost:** a comparison on the unit; for the fix, a
  `post-image.sh` change and an image build.
- **R2. Port `maia-kmod` patches 0002 to 0004** from Tezuka's `package/maia-kmod/` into the fork's
  package. The driver source is the same file (v0.10.0 and v0.12.0 are identical), and the three
  apply to it without 0001 (tried with `patch` in a scratch directory; not compiled). **Why:**
  they close races in the driver the three rings go through, and cost one image build. **Why not
  urgent:** the races need a second mapping or an unmap during an invalidate, and the scanner
  does neither.

### With the next bake

- **R3. Make the two Vivado projects stand on their own.** Copy what `fishball7020_p25` and
  `fishball7020_hwval` use from `projects/pluto/system_bd.tcl` into the fork's own files.
  **Why:** F5OEO's `main` has not moved in a year, `refactor` deletes that file, and the fork
  already deletes part of what the base design creates (the Maia core, the sweeper, the DDC's
  FIFOs and multiplexers). After that, upstream's project layout no longer matters, and the
  fork depends on upstream only for `maia_hdl`, the part it imports. **Cost:** a block-design
  rework and one full run of the gates.
- **R4. Give the lanes' mixer its 13th bit.** Two ways, the same arithmetic after the mixer:
  - take [PR 18](https://github.com/F5OEO/maia-sdr/pull/18) once F5OEO merges it (it edits
    `maia_hdl/ddc.py` and `mixer.py`, upstream's files);
  - or build `P25DDC` with `in_width=13` and sign-extend the 12-bit input, with no upstream
    file touched. This is a suggestion, not tested.

  **Cost:** one bit through the mixer, the skid buffer and the first FIR stage, and the lane
  model updated to match. The author's builds met timing on Pluto and Pluto+.
- **R5. Read two designs before choosing bake B's** (section 4): `juchong/maia-sdr`'s
  time-multiplexed lanes, and `earshot`'s filter bank with its resource figures. **Why:** they
  are the two ends of 079's choice (more DDC lanes, or step 3b's channelizer), both built and
  measured on a Zynq-7010. `earshot` is GPL-3.0, so it informs the design and no more.

### Watch

- **R6. F5OEO `refactor`**, for the day it becomes `main`, and for PR 12 (the complex FFT tap).
- **R7. Tezuka's kernel 6.12 and Buildroot 2026.05.** Rebase when something needs it: a driver, a
  package that no longer builds on 2025.05, or a new board. It is a change of its own: 14
  conflicts, new device trees, a new image layout, and every check on unit A again.
- **R8. U-Boot loading the bitstream from the card** (`7544da2`). A new bitstream would then ship
  as one file, without a new `BOOT.bin`; the bench's two images would differ in that file and
  the device tree only.
- **R9. chrony and PPS**, when the unit's clock is fixed.
- **R10. Tezuka's classifier**, when the survey work starts (section 4).
- **R11. ADI HDL.** The local pick is still needed: no ADI release carries `92534dc1d`.

### Ignore

- The original Maia's `maia-httpd` and `maia-wasm` releases: not in the scanner's image.
- F5OEO's new boards, DVB-S2 transmit, the sweeper, the IIO stream formats and the sync design.
- Tezuka's dashboard, DATV, SatDump, longmynd, WireGuard, MQTT and Tailscale work.
- The `-2` speed grade.

### Open questions for Andy

1. **Sync policy.** Keep `main` as a mirror of F5OEO's frozen `main`, or track `refactor` for
   `maia_hdl` once the Vivado projects stand alone (R3)?
2. **The submodule.** Push the cherry-pick to a fork of `analogdevicesinc/hdl` and point
   `.gitmodules` there, so that a fresh clone builds? It is a push, so it needs your say.
3. **The speed grade.** What is printed on the two units' Zynqs? Upstream assumes `-2` without
   knowing; the fork builds `-1`.
4. **The DRAM variant.** Upstream ships an underclocked FSBL for a Fishball with "bad DRAM". Do
   unit A and unit B pass `fbench`'s memory tests at the stock clock, and are they the same
   board revision?
5. **Giving back.** The kernel-size finding (if the check shows corruption on the SD path too)
   and a review of PR 18 would both help upstream. Nothing is pushed since 082, so this waits
   for you.
6. **A reference clock.** Is a GPS or PPS input planned (the panel board, the handheld)? It
   decides whether `refactor`'s oscillator core and upstream's PPS support matter.

## Not verified

- **Whether unit A's kernel is corrupted.** No unit was touched. The fork's boot path differs
  from the one upstream analysed.
- **The `maia-kmod` patches on kernel 6.1 and on hardware.** They apply to the source; they were
  not compiled here, and patch 0004's own text says it was untested when written.
- **The mixer's wrap on the fork's own bitstream.** It follows from the source the lanes
  instantiate and from the pull request's measurement on a Pluto+; it was not measured on a unit.
- **Fork branches beyond their last commit.** A branch was judged by its name, its distance from
  the parent and its last commit; only the branches named above were read further.
- **The surveyed projects' own claims.** None was built or run. For `earshot`, RadioFoundry
  Gateway and `tetra-zynq-phy` the README, the file list, the dates and the licence were read;
  for the rest of 4.2 only that the repository exists, its dates and its licence, with the
  description as a web survey reported it. RadioFoundry's 16 channels are its README's figure.
- **What a search can miss.** The searches covered names, descriptions and READMEs on GitHub and
  the open web; a project that uses none of the words there was not found. Two things could not
  be opened: the hardware details of the P25RX receivers, and a thesis on DMR 4FSK in an FPGA
  seen only as a search result.
- **Vivado 2025's hold problem** is upstream's account (`hold_fix.tcl`), not reproduced.
