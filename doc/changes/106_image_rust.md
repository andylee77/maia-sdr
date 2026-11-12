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
