# 106 — The image builds again, and the SD image with 096 to 105

**Date:** 2026-11-11. **Branch:** fishball-p25. **Bake required:** NO.

**Status:** committed locally.

## Why

Asked "Build an SD image with 096 to 105 and boot unit A from it?", Andy answered "Yes, build
it". The build failed at the scanner: `time` 0.3.55 and `time-core` 0.1.9, which 104's
certificate library (`rcgen`) brought in, need Rust 1.88, and the Tezuka image builds with Rust
1.86 ("rustc 1.86.0 is not supported by the following packages"). A PC's Rust is newer (1.93)
and builds them, so the host tests, the ARM check and the ARM release build had all passed, and
104's and 105's test builds ran on unit A: nothing on the PC could say the image would not
build. The manifest did say which Rust the image has (`rust-version = "1.86"`), and nothing read
it: with the manifest's edition, cargo takes the newest version of a dependency whatever Rust it
needs.

## Changes

- **`Cargo.lock`** holds `time` at 0.3.45, `time-core` at 0.1.7 and `num-conv` at 0.1.0: the
  newest that Rust 1.86 builds. Nothing else in the lock needed a newer Rust.
- **`scanner/.cargo/config.toml`** (new) has cargo resolve a dependency to the newest version
  the manifest's `rust-version` can build, so the next new dependency does not take versions the
  image cannot compile.
- **A host test** (`scanner/tests/image_rust.rs`, new) reads what the lock resolves to for the
  unit's target and fails, naming them, when a package needs a newer Rust than the manifest's.
  Run on the lock that broke the build, it names `time 0.3.55` and `time-core 0.1.9`.
- BUILD_TAG `2026-11-11-access2`, the image's.

What this does not catch: the scanner's own code using something of Rust newer than 1.86. Only
the image's compiler finds that; the image build is its check.

## Checks

- Host tests 582 pass (2 new); the DMR reference's tests pass (102); the ARM check is clean.
- The image build, and the image on unit A: the status log.

## Status log

- 2026-11-11: the first image build failed at the scanner, as above; with the lock fixed the
  second built in 5 min 5 s, the image's Rust 1.86 compiling the scanner in 3 min 41 s. So the
  scanner's own code of 096 to 105, which only a PC's Rust had compiled, builds with the
  image's too.
- 2026-11-11: the SD image `2026-11-11-access2` (fishball-p25 572e642, tezuka_fw d4d0eb2, the
  radio core 2.1.0's fifth build) on unit A, its boot files also in
  `/mnt/sd/boot_new_access2` (pages2's in `/mnt/sd/boot_new_pages2`). It is the first image
  with 096 to 106 and with the firmware's 007 and 008.
  - **Checked before installing** (the ramdisk read on the PC): the scanner carries the tag and
    has the certificate store, the plain port's redirect and libiio's switch, and no longer
    takes certificate files; `S60scanner` starts both ports; the old certificate script and its
    extension file are gone; `S50flash`, `S23udc` with the switch and `iio-switch` (mode 755,
    LF) are there. `BOOT.bin` differs from pages2's in 6 bytes, U-Boot's build time, so the
    bitstream is the same; the device tree and `uEnv.txt` are pages2's; the kernel was built
    again. The card's copies matched the build's sha256 before the reboot.
  - **After the reboot** (libiio kept off): the scanner started 53 s into the boot and was live
    on Clay County 102 s after the reboot was asked, control decode 100 % at 40 messages a
    second; 101,774 recordings listed; open as before. HTTPS verified against the CA file saved
    before the reboot: the image went on with the unit's CA and its certificate, which a start
    in 1970 did not make again. No `iiod`, port 30431 refusing, the gadget on the bus without
    an IIO function, `usb0` at its address, the console on `ttyGS0`.
  - **The checks against the image:** `iiod_api.py --unit` 10 steps; with A serving the page,
    reads only, over HTTP: `tool_pages.mjs` 27, `files_page.mjs` 9, `now_rail.mjs` 15,
    `aliases_page.mjs` 9, `playlist_page.mjs --preview-only` 13, `scanner_pages.mjs` 40; over
    HTTPS: `tool_pages.mjs` 27, `files_page.mjs` 9, `now_rail.mjs` 15.
  - **A second reboot with libiio kept on** (through the API): live again 104 s after it was
    asked, `iiod` running, the gadget built with its IIO function, `iio_attr` reading the board
    from the PC. Then switched off through the API: A is left with libiio off and the choice
    kept off, live on Clay at 100 %. `scanner/doc/API_FIELDS.md` regenerated from A.
- **Seen in the image, not changed** (they were in pages2 too): `S50maia-sdr-certificates`, the
  Maia image's first-boot certificate script, left in the build's target directory and run at
  each start (it made the `maia-httpd.*` files on A's flash; nothing reads them);
  `S99fishball_uhd` with CRLF line ends.
- **Not checked:** an image built from nothing (this build kept what its target directory had,
  the `openssl` binary among it); unit B, which is not in use and still has an older image; and
  what 104's and 105's records still list (a browser with the CA installed, a password on a
  unit, a host on the USB cable while libiio is switched).
