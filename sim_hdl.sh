#!/usr/bin/env bash
###############################################################################
# sim_hdl.sh — Maia HDL Simulation Runner (runs inside Docker)
#
# Runs HDL unit tests using Amaranth's Python simulator (Tier 1) and
# optionally cocotb + Icarus Verilog (Tier 2).
#
# Launched by sim_hdl.bat — do not run directly on Windows.
#
# Tier 1: Amaranth Python simulator (pytest)
#   - test/test_*.py — unit tests for HDL modules
#   - No extra tools needed, pure Python
#   - ~1-2 minutes
#
# Tier 2: cocotb + Icarus Verilog
#   - test_cocotb/*/ — co-simulation tests
#   - Requires iverilog (auto-installed if missing)
#   - ~5 minutes
#
# Usage:
#   bash sim_hdl.sh                  Run both tiers
#   bash sim_hdl.sh --tier1          Tier 1 only (fast, no iverilog needed)
#   bash sim_hdl.sh --tier2          Tier 2 only (cocotb)
#   bash sim_hdl.sh --test NAME      Run single test by name/substring
#   bash sim_hdl.sh --vcd            Save VCD waveforms
#
# Environment:
#   SRC_MOUNT    Path to maia-sdr repo root (Docker mount)
#   BUILD_HOME   /root/maia_hdl_build (Docker volume, ext4)
###############################################################################

set -euo pipefail

# ── Install rsync if missing ──────────────────────────────────────────────────
if ! command -v rsync &>/dev/null; then
    echo "[INFO] Installing rsync..."
    apt-get update -qq && apt-get install -y -qq rsync >/dev/null 2>&1
    echo "[OK] rsync installed."
fi

# ── Colour helpers ────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'
log()  { echo -e "${GREEN}[SIM]${NC} $*"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
err()  { echo -e "${RED}[ERROR]${NC} $*" >&2; }
info() { echo -e "${CYAN}[INFO]${NC} $*"; }
step() { echo -e "\n${BOLD}══ $* ══${NC}"; }

# ── Configuration ─────────────────────────────────────────────────────────────
BUILD_HOME="${BUILD_HOME:-/root/maia_hdl_build}"
SRC_MOUNT="${SRC_MOUNT:-/mnt/src}"

SRC_DIR="$BUILD_HOME/src"
VENV_DIR="$BUILD_HOME/venv"

# Flags
RUN_TIER1=true
RUN_TIER2=true
SAVE_VCD=false
SINGLE_TEST=""

# ── Argument parsing ──────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --tier1)       RUN_TIER2=false; shift ;;
        --tier2)       RUN_TIER1=false; shift ;;
        --vcd)         SAVE_VCD=true; shift ;;
        --test)        SINGLE_TEST="$2"; shift 2 ;;
        --help|-h)
            grep '^#' "$0" | grep -v '^#!/' | sed 's/^# \{0,1\}//' | head -40
            exit 0 ;;
        *) err "Unknown option: $1"; exit 1 ;;
    esac
done

# ── Banner ────────────────────────────────────────────────────────────────────
echo ""
log "=== Maia HDL Simulation ==="
echo ""
info "Source:  $SRC_MOUNT/maia-hdl"
info "Build:   $SRC_DIR"
info "Tier 1:  $([ $RUN_TIER1 = true ] && echo 'Amaranth Python sim' || echo 'SKIP')"
info "Tier 2:  $([ $RUN_TIER2 = true ] && echo 'cocotb + Icarus' || echo 'SKIP')"
$SAVE_VCD && info "VCD:     enabled"
[ -n "$SINGLE_TEST" ] && info "Filter:  $SINGLE_TEST"
echo ""

# ── Validate source ──────────────────────────────────────────────────────────
if [ ! -d "$SRC_MOUNT/maia-hdl/maia_hdl" ]; then
    err "maia-hdl source not found at: $SRC_MOUNT/maia-hdl/maia_hdl/"
    exit 1
fi

# ── Sync source to ext4 ──────────────────────────────────────────────────────
step "Syncing source to ext4"
mkdir -p "$SRC_DIR"

rsync -a --delete \
    --exclude='*.pyc' \
    --exclude='__pycache__/' \
    --exclude='*.egg-info/' \
    --exclude='adi-hdl/' \
    --exclude='XilinxUnisimLibrary/' \
    --exclude='projects/' \
    "$SRC_MOUNT/maia-hdl/" "$SRC_DIR/"

log "Source synced."

# ── Bootstrap venv if needed ──────────────────────────────────────────────────
if [ ! -d "$VENV_DIR" ]; then
    step "Bootstrap: Creating Python venv"

    PYTHON_BIN=""
    for py in python3.11 python3.10 python3.9 python3; do
        if command -v "$py" &>/dev/null; then
            VER=$("$py" -c 'import sys; print(sys.version_info >= (3,9))')
            if [ "$VER" = "True" ]; then
                PYTHON_BIN="$py"
                break
            fi
        fi
    done
    if [ -z "$PYTHON_BIN" ]; then
        err "Python 3.9+ required but not found."
        exit 1
    fi

    "$PYTHON_BIN" -m venv "$VENV_DIR"
    source "$VENV_DIR/bin/activate"
    pip install --quiet --upgrade pip
    pip install --quiet "amaranth>=0.5,<0.6" numpy scipy amaranth-yosys pytest
    log "Venv created and packages installed."
else
    source "$VENV_DIR/bin/activate"
    # Ensure pytest is available
    pip install --quiet pytest 2>/dev/null || true
    log "Venv active: $(python --version)"
fi

# Install maia_hdl package
pip install --quiet -e "$SRC_DIR"

# ── TIER 1: Amaranth Python simulator ─────────────────────────────────────────
TIER1_PASS=true
TIER1_RESULTS=""

if $RUN_TIER1; then
    step "Tier 1: Amaranth Python Simulator"
    info "Tests: test/test_*.py"
    echo ""

    cd "$SRC_DIR"

    # Build pytest arguments
    PYTEST_ARGS="-v --tb=short"
    TEST_SPEC="test/"

    if [ -n "$SINGLE_TEST" ]; then
        PYTEST_ARGS="$PYTEST_ARGS -k $SINGLE_TEST"
    fi

    if $SAVE_VCD; then
        VCD_DIR="$SRC_MOUNT/sim_output/vcd_tier1"
        mkdir -p "$VCD_DIR"
        export SIM_VCD_DIR="$VCD_DIR"
        info "VCD output: $VCD_DIR"
    fi

    # Run tests
    set +e
    python -m pytest $PYTEST_ARGS $TEST_SPEC 2>&1
    TIER1_EXIT=$?
    set -e

    if [ $TIER1_EXIT -eq 0 ]; then
        log "✓ Tier 1 PASSED"
        TIER1_RESULTS="PASS"
    elif [ $TIER1_EXIT -eq 5 ]; then
        warn "No tests collected (test files may not exist yet)"
        TIER1_RESULTS="NO TESTS"
    else
        err "✗ Tier 1 FAILED (exit code $TIER1_EXIT)"
        TIER1_PASS=false
        TIER1_RESULTS="FAIL"
    fi
fi

# ── TIER 2: cocotb + Icarus Verilog ──────────────────────────────────────────
TIER2_PASS=true
TIER2_RESULTS=""

if $RUN_TIER2; then
    step "Tier 2: cocotb + Icarus Verilog"

    # Check/install iverilog
    if ! command -v iverilog &>/dev/null; then
        warn "iverilog not found. Installing..."
        apt-get update -qq && apt-get install -y -qq iverilog >/dev/null 2>&1 || {
            err "Could not install iverilog. Skipping tier 2."
            TIER2_RESULTS="SKIP (no iverilog)"
            TIER2_PASS=true
            RUN_TIER2=false
        }
    fi

    if $RUN_TIER2; then
        # Check/install cocotb
        if ! python -c "import cocotb" 2>/dev/null; then
            log "Installing cocotb + cocotb-bus..."
            pip install --quiet cocotb cocotb-bus
        fi

        # Find cocotb test directories (each has a Makefile)
        COCOTB_DIR="$SRC_DIR/test_cocotb"
        if [ -d "$COCOTB_DIR" ]; then
            COCOTB_TESTS=$(find "$COCOTB_DIR" -name "Makefile" -not -path "$COCOTB_DIR/Makefile" | sort)

            if [ -z "$COCOTB_TESTS" ]; then
                warn "No cocotb test directories found."
                TIER2_RESULTS="NO TESTS"
            else
                TIER2_FAIL_COUNT=0
                TIER2_TOTAL=0

                for makefile in $COCOTB_TESTS; do
                    TEST_DIR=$(dirname "$makefile")
                    TEST_NAME=$(basename "$TEST_DIR")
                    TIER2_TOTAL=$((TIER2_TOTAL + 1))

                    info "Running: $TEST_NAME"
                    cd "$TEST_DIR"

                    set +e
                    make SIM=icarus 2>&1 | tail -20
                    if [ ${PIPESTATUS[0]} -ne 0 ]; then
                        err "  $TEST_NAME FAILED"
                        TIER2_FAIL_COUNT=$((TIER2_FAIL_COUNT + 1))
                    else
                        log "  $TEST_NAME PASSED"
                    fi
                    set -e
                done

                if [ $TIER2_FAIL_COUNT -eq 0 ]; then
                    TIER2_RESULTS="PASS ($TIER2_TOTAL tests)"
                else
                    TIER2_RESULTS="FAIL ($TIER2_FAIL_COUNT/$TIER2_TOTAL failed)"
                    TIER2_PASS=false
                fi
            fi
        else
            warn "test_cocotb/ directory not found."
            TIER2_RESULTS="SKIP (no test_cocotb/)"
        fi
    fi
fi

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
step "Simulation Results"
echo ""

if [ "$RUN_TIER1" = true ] || [ -n "$TIER1_RESULTS" ]; then
    if $TIER1_PASS; then
        echo -e "  Tier 1 (Amaranth Python sim):  ${GREEN}${TIER1_RESULTS}${NC}"
    else
        echo -e "  Tier 1 (Amaranth Python sim):  ${RED}${TIER1_RESULTS}${NC}"
    fi
fi
if [ -n "$TIER2_RESULTS" ]; then
    if $TIER2_PASS; then
        echo -e "  Tier 2 (cocotb + iverilog):    ${GREEN}${TIER2_RESULTS}${NC}"
    else
        echo -e "  Tier 2 (cocotb + iverilog):    ${RED}${TIER2_RESULTS}${NC}"
    fi
fi
echo ""

# Exit with failure if any tier failed
if ! $TIER1_PASS || ! $TIER2_PASS; then
    exit 1
fi
