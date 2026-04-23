#!/usr/bin/env bash
#
# Clean-output wrapper around the Tezuka firmware build.
#
# Runs `build.bat --p25` in the Tezuka repo with its full output
# captured to tezuka_build.log, AND pipes through
# tools/build_progress.py for live milestone output.
#
# Fresh build = 1-3 hours; cached rebuild = 20-40 minutes. Most of
# that time is spent inside Buildroot running per-package actions;
# the progress filter shows one line per significant package
# transition (Building, Installing to target) rather than every
# Extract/Patch/Configure/Stage step.
#
# Usage:
#   ./build_tezuka_p25_pretty.sh
#
# Env overrides:
#   TEZUKA_FW   default: C:\Users\Andy\Projects\Tezuka\tezuka_fw
#   LOG_FILE    default: tezuka_build.log (in maia-sdr repo)
#
# Post-build output images live at:
#   $TEZUKA_FW/output_images/sdimg/

set -eu -o pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEZUKA_FW="${TEZUKA_FW:-C:/Users/Andy/Projects/Tezuka/tezuka_fw}"
LOG_FILE="${LOG_FILE:-$SCRIPT_DIR/tezuka_build.log}"

if [ ! -f "$TEZUKA_FW/build.bat" ]; then
    echo "ERROR: $TEZUKA_FW/build.bat not found." >&2
    echo "       Set TEZUKA_FW to the tezuka_fw repo path." >&2
    exit 2
fi

echo "tezuka fw: $TEZUKA_FW"
echo "log file:  $LOG_FILE"
echo ""

# build.bat lives in the tezuka_fw repo. Run it from there so its
# relative paths resolve, tee output to a log, pipe through the
# progress filter in --mode tezuka.
(
    cd "$TEZUKA_FW"
    ./build.bat --p25 2>&1
) \
    | tee "$LOG_FILE" \
    | python "$SCRIPT_DIR/tools/build_progress.py" --mode tezuka

BAT_STATUS=${PIPESTATUS[0]}
if [ "$BAT_STATUS" -ne 0 ]; then
    echo "TEZUKA BUILD FAILED (exit $BAT_STATUS) -- see $LOG_FILE" >&2
    exit "$BAT_STATUS"
fi
