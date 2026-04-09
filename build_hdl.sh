#!/usr/bin/env bash
###############################################################################
# build_hdl.sh — Maia HDL Verilog + SVD Generator (runs inside Docker)
#
# Generates maia_sdr.v (Verilog) and maia-sdr.svd (register map) from the
# Amaranth HDL Python source. Runs on a Docker ext4 filesystem to avoid
# Windows NTFS/CIFS issues with pip editable installs and symlinks.
#
# Launched by build_hdl.bat — do not run directly on Windows.
#
# Usage:
#   bash build_hdl.sh [options]
#
# Options:
#   --config <name>    Amaranth config (default: maia_iio)
#   --verilog-only     Skip SVD generation
#   --svd-only         Skip Verilog generation
#   --clean            Remove cached venv and rebuild from scratch
#   --interactive      (handled by .bat — ignored here)
#
# Environment:
#   SRC_MOUNT    Path to maia-sdr repo root (Docker mount)
#   BUILD_HOME   /root/maia_hdl_build (Docker volume, ext4)
#
# Outputs (copied back to SRC_MOUNT):
#   maia-hdl/ip/maia-sdr/<config>/maia_sdr.v
#   maia-hdl/maia-sdr.svd
###############################################################################

set -euo pipefail

# ── Install rsync if missing (python:3.11-slim doesn't have it) ──────────────
if ! command -v rsync &>/dev/null; then
    echo "[INFO] Installing rsync..."
    apt-get update -qq && apt-get install -y -qq rsync >/dev/null 2>&1
    echo "[OK] rsync installed."
fi

# ── Colour helpers ────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
CYAN='\033[0;36m'; BOLD='\033[1m'; NC='\033[0m'
log()  { echo -e "${GREEN}[HDL]${NC} $*"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
err()  { echo -e "${RED}[ERROR]${NC} $*" >&2; }
info() { echo -e "${CYAN}[INFO]${NC} $*"; }
step() { echo -e "\n${BOLD}══ $* ══${NC}"; }

# ── Configuration ─────────────────────────────────────────────────────────────
BUILD_HOME="${BUILD_HOME:-/root/maia_hdl_build}"
SRC_MOUNT="${SRC_MOUNT:-/mnt/src}"

CONFIG="maia_iio"
DO_CLEAN=false
DO_VERILOG=true
DO_SVD=true
DO_P25=false
P25_CONFIG="default"

# Build dir layout (all on ext4)
SRC_DIR="$BUILD_HOME/src"
VENV_DIR="$BUILD_HOME/venv"

# ── Argument parsing ──────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --config)        CONFIG="$2"; shift 2 ;;
        --clean)         DO_CLEAN=true; shift ;;
        --verilog-only)  DO_SVD=false; shift ;;
        --svd-only)      DO_VERILOG=false; shift ;;
        --p25)           DO_P25=true; shift ;;
        --p25-config)    P25_CONFIG="$2"; shift 2 ;;
        --interactive)   shift ;;  # handled by .bat
        --help|-h)
            grep '^#' "$0" | grep -v '^#!/' | sed 's/^# \{0,1\}//' | head -40
            exit 0 ;;
        *) err "Unknown option: $1"; exit 1 ;;
    esac
done

# ── Banner ────────────────────────────────────────────────────────────────────
echo ""
log "=== Maia HDL Verilog + SVD Generator ==="
echo ""
info "Config:       $CONFIG"
info "Source:       $SRC_MOUNT/maia-hdl"
info "Build (ext4): $BUILD_HOME"
info "Verilog:      $($DO_VERILOG && echo 'yes' || echo 'skip')"
info "SVD:          $($DO_SVD && echo 'yes' || echo 'skip')"
info "P25:          $($DO_P25 && echo "yes (config: $P25_CONFIG)" || echo 'skip')"
echo ""

# ── Validate source ──────────────────────────────────────────────────────────
if [ ! -d "$SRC_MOUNT/maia-hdl/maia_hdl" ]; then
    err "maia-hdl source not found at: $SRC_MOUNT/maia-hdl/maia_hdl/"
    err "This script should be launched by build_hdl.bat"
    exit 1
fi

for f in maia_hdl/maia_sdr.py maia_hdl/config.py maia_hdl/configs.py pyproject.toml; do
    if [ ! -f "$SRC_MOUNT/maia-hdl/$f" ]; then
        err "Required source file missing: maia-hdl/$f"
        exit 1
    fi
done
log "Source files verified."

# Validate P25 source if building P25
if $DO_P25; then
    for f in p25_hdl/p25_top.py p25_hdl/c4fm_demod.py p25_hdl/symbol_timing.py p25_hdl/dibit_packer.py; do
        if [ ! -f "$SRC_MOUNT/maia-hdl/$f" ]; then
            err "Required P25 source file missing: maia-hdl/$f"
            exit 1
        fi
    done
    log "P25 source files verified."
fi

# ── Optional clean ────────────────────────────────────────────────────────────
if $DO_CLEAN; then
    step "Clean"
    warn "Removing ext4 build dir: $BUILD_HOME"
    rm -rf "$BUILD_HOME"
    log "Clean done."
fi

# ── Step 1: Copy source to ext4 ──────────────────────────────────────────────
step "Step 1: Copy source → ext4 ($SRC_DIR)"
info "Avoids CIFS/NTFS symlink and chmod issues during pip install"
mkdir -p "$SRC_DIR"

rsync -a --delete \
    --exclude='*.pyc' \
    --exclude='__pycache__/' \
    --exclude='*.egg-info/' \
    --exclude='.eggs/' \
    --exclude='dist/' \
    --exclude='build/' \
    --exclude='adi-hdl/' \
    --exclude='XilinxUnisimLibrary/' \
    --exclude='projects/' \
    --exclude='test_cocotb/' \
    "$SRC_MOUNT/maia-hdl/" "$SRC_DIR/"

log "Source copied to ext4."

# ── Step 2: Python virtual environment ────────────────────────────────────────
step "Step 2: Python virtualenv ($VENV_DIR)"

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
    err "Python 3.9+ is required but not found."
    exit 1
fi
info "Using: $PYTHON_BIN ($($PYTHON_BIN --version))"

if [ ! -d "$VENV_DIR" ]; then
    log "Creating new virtualenv..."
    "$PYTHON_BIN" -m venv "$VENV_DIR"
    log "Virtualenv created."
else
    log "Virtualenv already exists — reusing."
fi

source "$VENV_DIR/bin/activate"
log "Venv active: $(python --version)"

# ── Step 3: Install dependencies ─────────────────────────────────────────────
step "Step 3: Install Python dependencies"

pip install --quiet --upgrade pip
log "Installing amaranth>=0.5,<0.6 from PyPI..."
pip install --quiet "amaranth>=0.5,<0.6"
log "Installing numpy, scipy..."
pip install --quiet numpy scipy
log "Installing amaranth-yosys (Verilog backend)..."
pip install --quiet amaranth-yosys

# Install svd2rust for P25 PAC regeneration (cached in Docker volume)
if ! command -v svd2rust >/dev/null 2>&1; then
    log "Installing svd2rust (P25 PAC generator)..."
    apt-get install -y -qq curl >/dev/null 2>&1 || true
    SVD2RUST_VERSION="0.33.5"
    SVD2RUST_URL="https://github.com/rust-embedded/svd2rust/releases/download/v${SVD2RUST_VERSION}/svd2rust-x86_64-unknown-linux-gnu.gz"
    if curl -sL "$SVD2RUST_URL" -o /tmp/svd2rust.gz 2>/dev/null && \
       gunzip -f /tmp/svd2rust.gz && \
       chmod +x /tmp/svd2rust && \
       mv /tmp/svd2rust /usr/local/bin/svd2rust; then
        log "  svd2rust v${SVD2RUST_VERSION} installed"
    else
        warn "svd2rust download failed - PAC regeneration must be done manually"
    fi
fi

# ── Step 4: Install maia_hdl package ──────────────────────────────────────────
step "Step 4: Install maia_hdl package (editable)"
pip install --quiet -e "$SRC_DIR"
log "maia_hdl installed: $(pip show maia_hdl 2>/dev/null | grep '^Version:' || echo 'unknown')"

# ── Step 5: Generate Verilog ──────────────────────────────────────────────────
if $DO_VERILOG; then
    step "Step 5: Generate Verilog → maia_sdr.v"
    cd "$SRC_DIR"
    info "Command: python -m maia_hdl.maia_sdr --config $CONFIG maia_sdr.v"

    python -m maia_hdl.maia_sdr --config "$CONFIG" maia_sdr.v

    if [ ! -f maia_sdr.v ]; then
        err "maia_sdr.v was not created — Amaranth elaboration failed."
        exit 1
    fi

    V_LINES=$(wc -l < maia_sdr.v)
    log "Verilog generated: maia_sdr.v ($V_LINES lines)"
fi

# ── Step 5b: Generate P25 Verilog ─────────────────────────────────────────────
if $DO_P25; then
    step "Step 5b: Generate P25 Verilog → p25_core.v"
    cd "$SRC_DIR"
    info "Command: PYTHONPATH=. python -m p25_hdl.p25_top --config $P25_CONFIG p25_core.v"

    PYTHONPATH="." python -m p25_hdl.p25_top --config "$P25_CONFIG" p25_core.v

    if [ ! -f p25_core.v ]; then
        err "p25_core.v was not created — P25 Amaranth elaboration failed."
        exit 1
    fi

    P25_LINES=$(wc -l < p25_core.v)
    log "P25 Verilog generated: p25_core.v ($P25_LINES lines)"
fi

# ── Step 5c: Generate P25 SVD ─────────────────────────────────────────────────
# Always generate the P25 SVD when building P25 — it's tiny and the PAC
# depends on it. Avoids stale PAC after register map changes.
if $DO_P25; then
    step "Step 5c: Generate P25 SVD → p25.svd"
    cd "$SRC_DIR"

    PYTHONPATH="." python - <<PYEOF
import sys
from p25_hdl.p25_top import P25Core
from p25_hdl import configs

cfg_fn = getattr(configs, '${P25_CONFIG}', None)
if cfg_fn is None:
    print(f"[ERROR] No P25 config named '${P25_CONFIG}' in p25_hdl.configs", file=sys.stderr)
    sys.exit(1)

core = P25Core(cfg_fn())
svd_bytes = core.svd()

with open('p25.svd', 'wb') as f:
    f.write(svd_bytes)

import xml.etree.ElementTree as ET
tree = ET.fromstring(svd_bytes)
regs = tree.findall('.//register')
print(f'P25 SVD written: {len(svd_bytes)} bytes, {len(regs)} registers')
for r in regs:
    name = r.findtext('name', '')
    offset = r.findtext('addressOffset', '')
    print(f'  {offset}: {name}')
PYEOF

    if [ ! -f p25.svd ]; then
        err "p25.svd was not created — P25 SVD generation failed."
        exit 1
    fi
    log "P25 SVD generated: p25.svd ($(wc -c < p25.svd) bytes)"
fi

# ── Step 6: Generate SVD ──────────────────────────────────────────────────────
if $DO_SVD; then
    step "Step 6: Generate SVD → maia-sdr.svd"
    cd "$SRC_DIR"

    python - <<PYEOF
import sys
from maia_hdl.maia_sdr import MaiaSDR
from maia_hdl import configs

cfg_fn = getattr(configs, '${CONFIG}', None)
if cfg_fn is None:
    print(f"[ERROR] No config named '${CONFIG}' in maia_hdl.configs", file=sys.stderr)
    sys.exit(1)

top = MaiaSDR(cfg_fn())
svd_bytes = top.svd()

with open('maia-sdr.svd', 'wb') as f:
    f.write(svd_bytes)

import xml.etree.ElementTree as ET
tree = ET.fromstring(svd_bytes)
regs = tree.findall('.//register')
print(f'SVD written: {len(svd_bytes)} bytes, {len(regs)} registers')
for r in regs:
    name = r.findtext('name', '')
    offset = r.findtext('addressOffset', '')
    print(f'  {offset}: {name}')
PYEOF

    if [ ! -f maia-sdr.svd ]; then
        err "maia-sdr.svd was not created — SVD generation failed."
        exit 1
    fi
    log "SVD generated: maia-sdr.svd ($(wc -c < maia-sdr.svd) bytes)"
fi

# ── Step 7: Copy outputs back to source mount ─────────────────────────────────
step "Step 7: Copy outputs → source mount"

if $DO_VERILOG; then
    # Copy to the IP core config directory
    IP_DIR="$SRC_MOUNT/maia-hdl/ip/maia-sdr/$CONFIG"
    mkdir -p "$IP_DIR"
    cp "$SRC_DIR/maia_sdr.v" "$IP_DIR/maia_sdr.v"
    log "  ✓ maia_sdr.v → maia-hdl/ip/maia-sdr/$CONFIG/ ($(wc -l < "$SRC_DIR/maia_sdr.v") lines)"
fi

if $DO_SVD; then
    cp "$SRC_DIR/maia-sdr.svd" "$SRC_MOUNT/maia-hdl/maia-sdr.svd"
    log "  ✓ maia-sdr.svd → maia-hdl/ ($(wc -c < "$SRC_DIR/maia-sdr.svd") bytes)"
fi

if $DO_P25; then
    P25_IP_DIR="$SRC_MOUNT/maia-hdl/ip/p25-core/$P25_CONFIG"
    mkdir -p "$P25_IP_DIR"
    cp "$SRC_DIR/p25_core.v" "$P25_IP_DIR/p25_core.v"
    log "  ✓ p25_core.v → maia-hdl/ip/p25-core/$P25_CONFIG/ ($(wc -l < "$SRC_DIR/p25_core.v") lines)"

    # Copy P25 SVD into the p25-pac crate and regenerate the PAC if svd2rust
    # is available. Otherwise leave it for the host to run manually.
    P25_PAC_DIR="$SRC_MOUNT/p25-httpd/p25-pac"
    if [ -d "$P25_PAC_DIR" ]; then
        cp "$SRC_DIR/p25.svd" "$P25_PAC_DIR/p25.svd"
        log "  ✓ p25.svd → p25-httpd/p25-pac/ ($(wc -c < "$SRC_DIR/p25.svd") bytes)"
        if command -v svd2rust >/dev/null 2>&1; then
            log "  Regenerating p25-pac with svd2rust..."
            (cd "$P25_PAC_DIR" && svd2rust -i p25.svd --target none && \
                mv lib.rs src/lib.rs 2>/dev/null) || \
                warn "svd2rust regeneration failed"
            log "  ✓ p25-pac src/lib.rs regenerated"
        else
            warn "svd2rust not in PATH — run manually:"
            warn "  cd p25-httpd/p25-pac && svd2rust -i p25.svd --target none && mv lib.rs src/lib.rs"
        fi
    fi
fi

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
log "=== BUILD COMPLETE ==="
echo ""
info "Output files:"
$DO_VERILOG && info "  maia-hdl/ip/maia-sdr/$CONFIG/maia_sdr.v"
$DO_SVD     && info "  maia-hdl/maia-sdr.svd"
$DO_P25     && info "  maia-hdl/ip/p25-core/$P25_CONFIG/p25_core.v"
echo ""
info "Next steps:"
if $DO_P25; then
    info "  1. Run build_fpga.bat --p25 to synthesize P25 FPGA bitstream"
else
    info "  1. Run build_fpga.bat to synthesize FPGA bitstream"
fi
if $DO_SVD; then
    info "  2. Regenerate Rust PAC from SVD:"
    info "     cd maia-httpd/maia-pac"
    info "     svd2rust -i ../../maia-hdl/maia-sdr.svd"
    info "     cargo fmt"
fi
echo ""
