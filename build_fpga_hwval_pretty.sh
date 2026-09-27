#!/usr/bin/env bash
#
# Clean-output wrapper around `build_fpga.bat --hwval` (hardware-validation
# bitstream, doc/HW_VALIDATION_SUITE.md section 11). Mirrors
# build_fpga_p25_pretty.sh.
#
# Runs the Vivado bake with its full output captured to bake_hwval.log,
# AND pipes through tools/build_progress.py to print one line per
# milestone in real time.
#
# Usage:
#   ./build_fpga_hwval_pretty.sh
#
# After the build, the raw log is at ./bake_hwval.log and the impl-run
# log is at
# maia-hdl/projects/fishball7020_hwval/fishball_hwval.runs/impl_1/runme.log.
#
# Exit code mirrors build_fpga.bat. For hwval a timing failure is a
# hard error (no system_top_bad_timing.xsa promotion). The progress
# filter never hides errors; any ERROR / CRITICAL WARNING lines are
# printed verbatim.
#
# See tools/build_progress.py for the phase patterns recognised.

set -eu -o pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

LOG_FILE="${BAKE_LOG:-bake_hwval.log}"

# Tee raw Vivado output to the log + the progress filter. The
# trailing `exit ${PIPESTATUS[0]}` propagates the batfile's exit code
# (not the progress filter's, which always exits 0 normally).
./build_fpga.bat --hwval 2>&1 \
    | tee "$LOG_FILE" \
    | python tools/build_progress.py

BAT_STATUS=${PIPESTATUS[0]}
if [ "$BAT_STATUS" -ne 0 ]; then
    echo "BAKE FAILED (exit $BAT_STATUS) -- see $LOG_FILE for details" >&2
    exit "$BAT_STATUS"
fi
