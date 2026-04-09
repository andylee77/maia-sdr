# 009 -- Auto-regenerate stale IP Verilog in build_fpga.bat

**Date:** 2026-04-09
**Phase:** 5 (Post hardware bring-up, decode debug)
**Branch:** fishball-p25

---

## Summary

`build_fpga.bat --p25` now detects when the Amaranth HDL source
(`p25_hdl/*.py`, `maia_hdl/*.py`) is newer than the generated
`ip/p25-core/default/p25_core.v` and automatically regenerates the
Verilog (via `build_hdl.bat --verilog-only --p25` in Docker) before
running Vivado synthesis. Previously the script only checked if the
`.v` file existed, silently baking stale logic into "fresh"-looking
bitstreams any time Amaranth source was edited without a manual
intermediate `build_hdl.bat` invocation.

The same check is applied to the Maia SDR Verilog
(`ip/maia-sdr/maia_iio/maia_sdr.v` vs `maia_hdl/*.py`).

## Motivation

Hit this during P25 decode debug on 2026-04-09. The symbol-rate slicer
fix (commits 3f1bff7 at 05:07 and 94faae9 at 05:27) was on disk and all
12 P25 HDL unit tests passed, but the on-target FPGA still showed the
exact pre-fix dibit histogram fingerprint:

```
Value 0 (+1): 70.3%
Value 1 (+3):  0.3%
Value 2 (-1): 29.2%
Value 3 (-3):  0.2%
Best sync Hamming: 37 / 48
```

That pattern matches the pre-fix failure mode recorded in the
`p25_top.py` docstring *verbatim*. After pulling file timestamps:

```
03:52:12  commit 8c704b4 "sign-bit dibit slicer"  ← SAMPLE-rate (failing)
03:52:45  ip/p25-core/default/p25_core.v         ← generated here
05:07:39  commit 3f1bff7 "move to symbol rate"   ← fix #1 (never in Verilog)
05:27:43  commit 94faae9 "register the multiplies" ← fix #2 (never in Verilog)
05:42:13  system_top.bit (Vivado output)         ← built from stale Verilog
08:40:27  BOOT.bin (Tezuka package)
```

Root cause: `build_fpga.bat --p25` only checked
`if exist p25_core.v` in its Verilog generation step. Once the file
existed, any subsequent Amaranth source edits were invisible to the
build script. Vivado happily synthesised the old Verilog, Tezuka
happily packaged the resulting XSA, and the downstream image looked
completely up-to-date by mtime — except for the fact that the FPGA
was running logic from 90 minutes before the fix.

Wasted considerable hardware-debug time chasing AD9361 DC offset
hypotheses, DDC NCO programming, and slicer pipeline correctness,
when the actual issue was that the slicer fix wasn't in the bitstream.

## Fix

### `tools/check_verilog_stale.ps1` (new)

A small PowerShell helper that compares the mtime of a generated
`.v` file against the maximum mtime of all `*.py` files under one
or more source directories. Outputs a single word on stdout:

- `MISSING` — generated file does not exist
- `STALE`   — at least one source file is newer
- `FRESH`   — generated file is newer than every source

Called from `build_fpga.bat` via:

```bat
powershell -NoProfile -ExecutionPolicy Bypass -File check_verilog_stale.ps1 `
    -VerilogFile "<path\to\generated.v>" `
    -SourceDirs  "<dir1>[;<dir2>...]"
```

`-SourceDirs` takes a semicolon-delimited single string rather than a
PowerShell array because PowerShell's `-File` mode can't bind
`[string[]]` from command-line arguments the way `-Command` can.

### `build_fpga.bat` Step 2 (modified)

The Verilog-existence check is replaced with a staleness check for
both IP cores:

- **Maia SDR Verilog:** compared against `maia-hdl/maia_hdl/*.py`
- **P25 Verilog:** compared against **both** `maia-hdl/p25_hdl/*.py`
  and `maia-hdl/maia_hdl/*.py` (P25 imports DDC, registers, DMA, and
  CDC modules from `maia_hdl`, so a change in either directory
  affects the generated `p25_core.v`)

When the check reports `MISSING` or `STALE`, the script automatically
calls `build_hdl.bat --verilog-only` (or `... --p25`) to regenerate
the IP Verilog, and aborts with a clear error message if regeneration
fails.

No user-facing API change: `build_fpga.bat --p25` is still the single
command users run. The `--verilog-only` flag on `build_hdl.bat`
remains as the internal mechanism for "emit Verilog without packaging
the IP," but users no longer need to know about it.

## Files Changed

| File | Change |
|------|--------|
| `tools/check_verilog_stale.ps1` | **New.** Staleness check helper for Amaranth→Verilog generation |
| `build_fpga.bat` | Step 2 now uses staleness check instead of file-existence check for both Maia and P25 IP Verilog |

## Verification

Confirmed the helper correctly distinguishes all three states on the
pre-fix state of the tree:

```
$ powershell -File tools/check_verilog_stale.ps1 `
    -VerilogFile "maia-hdl/ip/p25-core/default/p25_core.v" `
    -SourceDirs  "maia-hdl/p25_hdl;maia-hdl/maia_hdl"
STALE
```

And after running `build_fpga.bat --p25`, Step 2 reported:

```
[Step 2] Checking Verilog generation status...
[OK] maia_sdr.v is current (newer than maia_hdl/*.py).
[WARN] p25_core.v is STALE -- p25_hdl or maia_hdl has newer changes.
       Regenerating via Docker to avoid baking stale logic into bitstream.
```

followed by the Docker-based Verilog regeneration and a clean Vivado
synthesis run producing a fresh `.bit` containing the symbol-rate
slicer fix.

## Follow-ups Considered

1. **Pre-commit guard.** Could add a git pre-commit hook that
   refuses to commit `p25_hdl/*.py` changes without a matching
   `p25_core.v` regeneration. Rejected as too intrusive — the
   regeneration is slow (~30 s Docker overhead) and offline editing
   is common.

2. **Vivado project cache invalidation.** When IP Verilog
   regenerates, the project's `fishball_p25.gen/.../ipshared/*.v`
   staging copies become stale. In practice Vivado's IP hash-based
   cache key covers this correctly — content hash changes when the
   Verilog changes, so the synthesis cache entry is recomputed. No
   additional invalidation needed.

3. **Tezuka package cache.** Already handled in change 008 via the
   XSA mtime comparison in `tezuka_fw/build.sh`. No action needed
   here.

4. **Extend to traffic channel Verilog.** There is only one P25
   Verilog file (`p25_core.v`) covering both control and traffic
   chains, so this single staleness check is sufficient.
