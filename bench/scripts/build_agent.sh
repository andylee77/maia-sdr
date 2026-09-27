#!/usr/bin/env bash
# Builds the fbench-agent static ARM binary (armv7-unknown-linux-musleabihf,
# cargo-zigbuild) into bench/agent/dist/fbench-agent and, when docker with
# the arm32v7/debian:bullseye-slim image is available, runs a qemu smoke
# test and validates every JSON reply on the host.
#
# Usage: bench/scripts/build_agent.sh [--no-smoke] [--no-test]
#   --no-test   skip `cargo test` on the host
#   --no-smoke  skip the qemu smoke test
#
# Environment overrides: VENV (python venv with ziglang + cargo-zigbuild),
# CARGO_ZIGBUILD, CARGO_ZIGBUILD_ZIG_PATH, SMOKE_IMAGE.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
AGENT="$REPO/bench/agent"
TARGET=armv7-unknown-linux-musleabihf
VENV="${VENV:-$REPO/.venv-hdl}"
SMOKE_IMAGE="${SMOKE_IMAGE:-arm32v7/debian:bullseye-slim}"
DO_SMOKE=1
DO_TEST=1
for a in "$@"; do
    case "$a" in
        --no-smoke) DO_SMOKE=0 ;;
        --no-test) DO_TEST=0 ;;
        *) echo "unknown option $a" >&2; exit 2 ;;
    esac
done

winpath() {
    if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else echo "$1"; fi
}

# ── toolchain ─────────────────────────────────────────────────────────
if [[ -z "${CARGO_ZIGBUILD:-}" ]]; then
    if [[ -x "$VENV/Scripts/cargo-zigbuild.exe" ]]; then
        CARGO_ZIGBUILD="$VENV/Scripts/cargo-zigbuild.exe"
    elif [[ -x "$VENV/bin/cargo-zigbuild" ]]; then
        CARGO_ZIGBUILD="$VENV/bin/cargo-zigbuild"
    else
        CARGO_ZIGBUILD="$(command -v cargo-zigbuild || true)"
    fi
fi
if [[ -z "$CARGO_ZIGBUILD" ]]; then
    echo "cargo-zigbuild not found (pip install cargo-zigbuild ziglang into $VENV)" >&2
    exit 3
fi
if [[ -z "${CARGO_ZIGBUILD_ZIG_PATH:-}" ]]; then
    for z in "$VENV/Lib/site-packages/ziglang/zig.exe" "$VENV"/lib/python3*/site-packages/ziglang/zig; do
        if [[ -e "$z" ]]; then
            CARGO_ZIGBUILD_ZIG_PATH="$(winpath "$z")"
            break
        fi
    done
fi
export CARGO_ZIGBUILD_ZIG_PATH="${CARGO_ZIGBUILD_ZIG_PATH:-}"

cd "$AGENT"
if [[ $DO_TEST -eq 1 ]]; then
    echo "== cargo test (host)"
    cargo test --quiet 2>&1 | tail -n 20
fi

echo "== cargo zigbuild --release --target $TARGET"
"$CARGO_ZIGBUILD" zigbuild --release --target "$TARGET"
BIN="$AGENT/target/$TARGET/release/fbench-agent"
mkdir -p "$AGENT/dist"
cp -f "$BIN" "$AGENT/dist/fbench-agent"
SIZE=$(wc -c < "$AGENT/dist/fbench-agent" | tr -d ' ')
echo "== built $AGENT/dist/fbench-agent ($SIZE bytes)"
if command -v file >/dev/null 2>&1; then file "$AGENT/dist/fbench-agent"; fi

if [[ $DO_SMOKE -eq 0 ]]; then
    exit 0
fi
if ! command -v docker >/dev/null 2>&1 || ! docker image inspect "$SMOKE_IMAGE" >/dev/null 2>&1; then
    echo "== smoke test skipped (docker or $SMOKE_IMAGE not available)"
    exit 0
fi

# ── qemu smoke test ───────────────────────────────────────────────────
SMOKE="$AGENT/dist/smoke"
rm -rf "$SMOKE"
mkdir -p "$SMOKE/out"
cat > "$SMOKE/run.sh" <<'EOF'
#!/bin/sh
# Runs inside the arm32v7 container; the dist dir is mounted at /b.
A=/b/fbench-agent
O=/b/smoke/out
mkdir -p "$O" /tmp/fbench_smoke
run() {
    name=$1; shift
    "$A" "$@" > "$O/$name.json" 2> "$O/$name.err"
    echo $? > "$O/$name.rc"
}
run version version
run help help
run info info
run audit audit
run tx_off tx off
run maint_status maint status
run reg_list reg list
run reg_p25_list reg list --core p25
run reg_vacant reg read --core p25 --reg 0x120
run reg_nomap reg read --core bogus --reg X
run reg_rtc reg read --core p25 --reg wideband_iq_dma_status
run reg_ro_write reg write --core slcr --reg DDR_PLL_CTRL --value 0
run reg_p25_absent reg read --core p25 --reg product_id
run bad_cmd frobnicate
run bad_opt version --bogus 1
run mem_test mem test --anon-mb 8 --passes 1
run mem_phys_refused mem test --phys 0x00100000 --size 4096
run telemetry telemetry --seconds 1 --interval-ms 250
run telemetry_jsonl telemetry --seconds 1 --interval-ms 500 --jsonl /tmp/fbench_smoke/telemetry.jsonl
run sd_bench sd bench --mb 8 --bs 256 --fsync --dir /tmp/fbench_sd
run sd_refused sd bench --mb 1 --dir /etc/fbench
for p in ramp64 tagged prbs31 iqramp tone; do
    run "synth_$p" ring synth --pattern "$p" --out "/tmp/fbench_smoke/$p" --subbufs 16 --subbuf-bytes 64k --ring-subbufs 4
    run "check_$p" ring check --file "/tmp/fbench_smoke/$p.sigmf-data"
done
run synth_pn0fn ring synth --pattern pn0fn --out /tmp/fbench_smoke/pn0fn --subbufs 16 --subbuf-bytes 256k --ring-subbufs 16
run check_pn0fn ring check --file /tmp/fbench_smoke/pn0fn.sigmf-data
run synth_clean ring synth --pattern prbs31 --inject none --out /tmp/fbench_smoke/clean --subbufs 8
run check_clean ring check --file /tmp/fbench_smoke/clean.sigmf-data
run synth_custom ring synth --pattern ramp64 --inject "lap@3:2,lap@5:1,gap@6:100:3" --out /tmp/fbench_smoke/custom --subbufs 8 --subbuf-bytes 64k --ring-subbufs 4
run check_custom ring check --file /tmp/fbench_smoke/custom.sigmf-data
run ring_live ring check --ring p25-wideband --pattern pn0fn --seconds 1
run eyescan eyescan --mode idelay --dwell-ms 1
run boot_status boot status
run hwval_id hwval id
( "$A" net serve --port 5299 --timeout-s 20 > "$O/net_serve.json" 2> "$O/net_serve.err"; echo $? > "$O/net_serve.rc" ) &
sleep 1
run net_send net send --host 127.0.0.1 --port 5299 --mb 16
wait
exit 0
EOF
chmod +x "$SMOKE/run.sh"
echo "== qemu smoke test ($SMOKE_IMAGE)"
MSYS_NO_PATHCONV=1 docker run --rm -v "$(winpath "$AGENT/dist"):/b" "$SMOKE_IMAGE" /bin/sh /b/smoke/run.sh

PY="$(command -v python3 || command -v python || true)"
if [[ -z "$PY" ]]; then
    echo "== python not found: smoke outputs left in $SMOKE/out (not validated)"
    exit 0
fi
"$PY" - "$SMOKE/out" <<'EOF'
import json, os, sys

out = sys.argv[1]
fails = []

def load(name):
    with open(os.path.join(out, name + ".json"), encoding="utf-8") as f:
        text = f.read()
    lines = [l for l in text.splitlines() if l.strip()]
    if len(lines) != 1:
        fails.append(f"{name}: expected exactly one JSON line on stdout, got {len(lines)}")
    obj = json.loads(lines[-1]) if lines else {}
    rc = int(open(os.path.join(out, name + ".rc")).read().strip())
    if obj.get("ok") is True and rc != 0:
        fails.append(f"{name}: ok=true but rc={rc}")
    if obj.get("ok") is False and rc == 0:
        fails.append(f"{name}: ok=false but rc=0")
    return obj, rc

def expect(name, ok, code=None, rc=None):
    obj, r = load(name)
    if obj.get("ok") is not ok:
        fails.append(f"{name}: ok={obj.get('ok')} (want {ok}): {obj.get('error')}")
    if code is not None and obj.get("code") != code:
        fails.append(f"{name}: code={obj.get('code')} (want {code})")
    if rc is not None and r != rc:
        fails.append(f"{name}: rc={r} (want {rc})")
    return obj

v = expect("version", True, rc=0)
print(f"   version {v.get('version')} git {v.get('git')} target {v['build'].get('target')}")
expect("help", True)
info = expect("info", True)
print(f"   info: image={info.get('image')} kernel={info.get('kernel')} warnings={info.get('warnings')}")
expect("audit", True)
load("tx_off")
expect("maint_status", True)
expect("reg_list", True)
expect("reg_p25_list", True)
expect("reg_vacant", False, "safety", 4)
expect("reg_nomap", False, "not_found", 3)
expect("reg_rtc", False, "safety", 4)
expect("reg_ro_write", False, "safety", 4)
expect("reg_p25_absent", False, "wrong_image", 3)
expect("bad_cmd", False, "unknown_command", 3)
expect("bad_opt", False, "usage", 2)
m = expect("mem_test", True)
if m.get("errors") != 0:
    fails.append(f"mem_test: errors={m.get('errors')}")
print(f"   mem test: {m.get('bytes_tested')} bytes, {m.get('seconds')} s, errors {m.get('errors')}")
expect("mem_phys_refused", False)
t = expect("telemetry", True)
if t.get("count", 0) < 3:
    fails.append(f"telemetry: count={t.get('count')}")
tj = expect("telemetry_jsonl", True)
if tj.get("jsonl_lines", 0) < 1:
    fails.append("telemetry_jsonl: no lines written")
sd = expect("sd_bench", True)
if sd.get("verify_mismatches") != 0:
    fails.append("sd_bench: read-back mismatches")
print(f"   sd bench (container fs): write {sd.get('write_mbs')} MB/s read {sd.get('read_mbs')} MB/s p99 {sd.get('write_lat_us', {}).get('p99')} us")
expect("sd_refused", False, "safety", 4)
for p in ["ramp64", "tagged", "prbs31", "iqramp", "tone", "pn0fn", "clean", "custom"]:
    s = expect("synth_" + p, True)
    c = expect("check_" + p, True)
    want = s.get("expected_counts", {})
    got = c.get("counts", {})
    if got != want:
        fails.append(f"check_{p}: counts {got} != injected {want}")
    print(f"   ring check {p:7s}: {sum(got.values()):2d} anomalies, counts ok={got == want}, lost_units={c.get('lost_units')}")
cu = load("check_custom")[0]
if cu.get("lost_units") != (2 * 4 + 1 * 4) * 8192 + 3:
    fails.append(f"check_custom: lost_units={cu.get('lost_units')}")
expect("ring_live", False)
expect("eyescan", False)
expect("boot_status", True)
expect("hwval_id", False)
ns = expect("net_serve", True)
nd = expect("net_send", True)
if ns.get("bytes") != 16 << 20:
    fails.append(f"net_serve: bytes={ns.get('bytes')}")
print(f"   net loopback: {nd.get('mbs')} MB/s")
if fails:
    print("== SMOKE FAILURES:")
    for f in fails:
        print("   " + f)
    sys.exit(1)
print("== smoke test passed")
EOF
