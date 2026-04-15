"""
Voice-capture diagnostic monitor.

Waits for an unencrypted voice grant on the Fishball P25 target, captures
traffic-side metrics before/during/after the call, pulls the IMBE ring and
the resulting WAV, and writes everything to a timestamped dir.

Usage:
    python tools/voice_capture.py [TARGET] [MAX_WAIT_SECS]

Target defaults to 192.168.2.1:8080.
Max wait defaults to 900 (15 min).
"""
import json
import os
import subprocess
import sys
import time
import urllib.request

TARGET = sys.argv[1] if len(sys.argv) > 1 else "192.168.2.1:8080"
MAX_WAIT = int(sys.argv[2]) if len(sys.argv) > 2 else 900
POLL_MS = 50
MIN_LDU_DELTA = 10  # require at least this many new LDUs across a "caught" call

IIO_URI = f"ip:{TARGET.split(':')[0]}"
IQ_CAPTURE_SECS = 6          # cap per-call IQ capture
IQ_SAMPLE_RATE = 8_000_000
IQ_BUFFER_SIZE = 65536

OUT_DIR = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    f"voice_capture_{time.strftime('%Y%m%d_%H%M%S')}",
)
os.makedirs(OUT_DIR, exist_ok=True)


def log(msg):
    stamp = time.strftime("%H:%M:%S")
    line = f"[{stamp}] {msg}"
    print(line, flush=True)
    with open(os.path.join(OUT_DIR, "run.log"), "a", encoding="utf-8") as f:
        f.write(line + "\n")


def fetch_json(path, timeout=5):
    url = f"http://{TARGET}{path}"
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return json.loads(r.read())


def fetch_bytes(path, timeout=10):
    url = f"http://{TARGET}{path}"
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return r.read()


def save_json(name, obj):
    with open(os.path.join(OUT_DIR, name), "w", encoding="utf-8") as f:
        json.dump(obj, f, indent=2)


def save_bytes(name, data):
    with open(os.path.join(OUT_DIR, name), "wb") as f:
        f.write(data)


def snapshot(tag):
    snap = {
        "tag": tag,
        "t_wall": time.time(),
        "traffic": fetch_json("/api/traffic"),
        "decoder_compare": fetch_json("/api/decoder_compare"),
        "hdl_lsm": fetch_json("/api/hdl_lsm"),
        "stats": fetch_json("/api/stats"),
    }
    save_json(f"snapshot_{tag}.json", snap)
    return snap


def get_last_seq_in_log():
    d = fetch_json("/api/log?n=1")
    entries = d.get("entries", [])
    return entries[-1]["seq"] if entries else 0


def get_events_since(seq):
    d = fetch_json("/api/log?n=500")
    entries = d.get("entries", [])
    return [e for e in entries if e.get("seq", 0) > seq]


def delta_decoder_compare(a, b):
    """Compute delta metrics across a call window."""
    out = {}
    for chain in ("ps_lsm", "pl_hdl", "ps_c4fm"):
        ca, cb = a.get(chain, {}), b.get(chain, {})
        d = {}
        for key in (
            "nid_attempts", "nid_decoded_ok", "nid_decode_failures",
            "tsbk_block_attempts", "tsbk_crc_ok", "tsbk_crc_failures",
            "sync_hits", "sync_near", "total_dibits",
        ):
            va, vb = ca.get(key), cb.get(key)
            if isinstance(va, int) and isinstance(vb, int):
                d[key] = vb - va
        out[chain] = d
    return out


def delta_traffic(a, b):
    """Compute delta traffic-chain metrics."""
    keys = [
        ("imbe.ldu1_count", ("imbe", "ldu1_count")),
        ("imbe.ldu2_count", ("imbe", "ldu2_count")),
        ("imbe.hdu_count", ("imbe", "hdu_count")),
        ("imbe.tdu_count", ("imbe", "tdu_count")),
        ("imbe.tdu_lc_count", ("imbe", "tdu_lc_count")),
        ("imbe.imbe_frames_extracted", ("imbe", "imbe_frames_extracted")),
        ("imbe.imbe_frames_dropped", ("imbe", "imbe_frames_dropped")),
        ("imbe.imbe_frames_dropped_idle", ("imbe", "imbe_frames_dropped_idle")),
        ("imbe.vocoder_errors", ("imbe", "vocoder_errors")),
        ("imbe.vocoder_pcm_produced", ("imbe", "vocoder_pcm_produced")),
        ("imbe.vocoder_frames_encrypted", ("imbe", "vocoder_frames_encrypted")),
        ("tlsm_decoder.sync_hits", ("traffic_lsm_decoder", "sync_hits")),
        ("tlsm_decoder.sync_near_misses", ("traffic_lsm_decoder", "sync_near_misses")),
        ("tlsm_decoder.ldu1", ("traffic_lsm_decoder", "ldu1")),
        ("tlsm_decoder.ldu2", ("traffic_lsm_decoder", "ldu2")),
        ("tlsm_decoder.hdu", ("traffic_lsm_decoder", "hdu")),
        ("tlsm_decoder.tdu", ("traffic_lsm_decoder", "tdu")),
        ("tlsm_decoder.tdu_lc", ("traffic_lsm_decoder", "tdu_lc")),
    ]
    out = {}
    for label, path in keys:
        va, vb = a, b
        ok = True
        for p in path:
            if isinstance(va, dict) and p in va:
                va = va[p]
            else:
                ok = False; break
        for p in path:
            if isinstance(vb, dict) and p in vb:
                vb = vb[p]
            else:
                ok = False; break
        if ok and isinstance(va, int) and isinstance(vb, int):
            out[label] = vb - va
    return out


def is_active_clear(t):
    """Return True if traffic is in Active on a non-encrypted call."""
    if t.get("state") not in ("Active", "Acquiring"):
        return False
    enc = t.get("current_call_encrypted")
    return enc is False  # exactly False, not None


def ldu_sum(t):
    imbe = t.get("imbe", {}) or {}
    return (imbe.get("ldu1_count", 0) or 0) + (imbe.get("ldu2_count", 0) or 0)


def start_iq_capture(secs):
    """Launch iio_readdev as a background subprocess writing IQ to iq.bin."""
    nsamples = int(secs * IQ_SAMPLE_RATE)
    out_path = os.path.join(OUT_DIR, "iq.bin")
    err_path = os.path.join(OUT_DIR, "iq.err")
    cmd = [
        "iio_readdev",
        "-u", IIO_URI,
        "-b", str(IQ_BUFFER_SIZE),
        "-s", str(nsamples),
        "cf-ad9361-lpc", "voltage0", "voltage1",
    ]
    log(f"launching iio_readdev: {secs}s, {nsamples} samples ({nsamples*4/1e6:.1f} MB)")
    out_fh = open(out_path, "wb")
    err_fh = open(err_path, "wb")
    proc = subprocess.Popen(cmd, stdout=out_fh, stderr=err_fh)
    # Also snapshot AD9361 tuning state at capture start for reproducibility.
    save_json("iq_meta.json", {
        "started_wall": time.time(),
        "nsamples": nsamples,
        "samplerate_hz": IQ_SAMPLE_RATE,
        "device": "cf-ad9361-lpc",
        "channels": ["voltage0 (I)", "voltage1 (Q)"],
        "format": "int16 LE interleaved (S12 sign-extended)",
        "size_bytes": nsamples * 4,
        "iio_uri": IIO_URI,
    })
    return proc, out_fh, err_fh


def wait_for_call_start(deadline):
    """Poll /api/traffic until we see a new clear call BEGIN (state flips
    from Idle to Acquiring/Active AND enc==False). Returns (pre_snap, t) or
    (None, None) on timeout."""
    last_state = None
    while time.time() < deadline:
        try:
            t = fetch_json("/api/traffic", timeout=3)
        except Exception as e:
            log(f"poll error: {e}")
            time.sleep(0.3)
            continue
        state = t.get("state")
        enc = t.get("current_call_encrypted")
        if state != last_state:
            log(f"state {last_state} -> {state}  enc={enc}  tg={t.get('current_talkgroup')}")
            last_state = state
        # Require fresh start: previous state was Idle, new state is
        # Acquiring or Active, and encryption flag is explicitly False.
        if is_active_clear(t):
            return t
        time.sleep(POLL_MS / 1000.0)
    return None


def wait_for_call_end(deadline):
    while time.time() < deadline:
        try:
            t = fetch_json("/api/traffic", timeout=3)
        except Exception:
            time.sleep(0.3); continue
        if t.get("state") == "Idle":
            return t
        time.sleep(POLL_MS / 1000.0)
    return None


def main():
    log(f"output dir: {OUT_DIR}")
    log(f"target: {TARGET}")
    log(f"max wait: {MAX_WAIT}s  poll: {POLL_MS}ms  min ldu delta: {MIN_LDU_DELTA}")

    seq0 = get_last_seq_in_log()
    init_snap = snapshot("init")

    deadline = time.time() + MAX_WAIT
    attempt = 0
    while time.time() < deadline:
        attempt += 1
        log(f"--- attempt {attempt}: waiting for clear call start ---")

        # First settle on Idle: make sure we don't catch a call already
        # in progress (that's the false-positive from the first run).
        while time.time() < deadline:
            try:
                t = fetch_json("/api/traffic", timeout=3)
            except Exception:
                time.sleep(0.3); continue
            if t.get("state") == "Idle":
                break
            time.sleep(0.1)

        pre_ldu = ldu_sum(init_snap["traffic"])
        pre_snap = snapshot(f"pre_call_{attempt}")
        pre_ldu = ldu_sum(pre_snap["traffic"])

        t = wait_for_call_start(deadline)
        if t is None:
            log("timed out waiting for clear call start")
            break

        log(f"CALL START  tg={t.get('current_talkgroup')} "
            f"freq={t.get('current_frequency_hz')} offset={t.get('last_offset_hz')}")
        # Re-snapshot AT the start of the call (tuning has just retuned)
        pre_snap = snapshot(f"pre_call_{attempt}")
        pre_ldu = ldu_sum(pre_snap["traffic"])
        iq_proc, iq_out_fh, iq_err_fh = start_iq_capture(IQ_CAPTURE_SECS)

        t = wait_for_call_end(deadline)
        post_snap = snapshot(f"post_call_{attempt}")
        post_ldu = ldu_sum(post_snap["traffic"])
        ldu_delta = post_ldu - pre_ldu
        log(f"CALL END    ldu_delta={ldu_delta}")

        # Wait for iio_readdev to finish
        try:
            rc = iq_proc.wait(timeout=IQ_CAPTURE_SECS + 5)
            iq_out_fh.close(); iq_err_fh.close()
            sz = os.path.getsize(os.path.join(OUT_DIR, "iq.bin"))
            log(f"iio_readdev done rc={rc} iq.bin={sz} bytes ({sz/4/IQ_SAMPLE_RATE:.2f}s)")
        except subprocess.TimeoutExpired:
            iq_proc.kill()
            iq_out_fh.close(); iq_err_fh.close()
            log("iio_readdev timed out; killed")

        if ldu_delta < MIN_LDU_DELTA:
            log(f"too few LDUs ({ldu_delta} < {MIN_LDU_DELTA}); re-arming")
            # remove the iq.bin so next attempt overwrites
            try: os.remove(os.path.join(OUT_DIR, "iq.bin"))
            except: pass
            continue

        # Got a real capture. Pull the artefacts.
        try:
            imbe = fetch_json("/api/imbe_dump")
            save_json(f"imbe_dump_{attempt}.json", imbe)
            log(f"imbe_dump: {imbe.get('count', '?')} frames")
        except Exception as e:
            log(f"imbe_dump error: {e}")
        try:
            wav = fetch_bytes("/api/audio_test")
            save_bytes(f"audio_test_{attempt}.wav", wav)
            log(f"audio_test.wav: {len(wav)} bytes")
        except Exception as e:
            log(f"audio_test error: {e}")
        try:
            evts = get_events_since(seq0)
            save_json(f"events_{attempt}.json", evts)
            log(f"events since start: {len(evts)}")
        except Exception as e:
            log(f"events error: {e}")
        save_json(f"delta_decoder_compare_{attempt}.json",
                  delta_decoder_compare(pre_snap["decoder_compare"],
                                        post_snap["decoder_compare"]))
        save_json(f"delta_traffic_{attempt}.json",
                  delta_traffic(pre_snap["traffic"], post_snap["traffic"]))
        log(f"ATTEMPT {attempt} DONE")
        return 0

    log(f"TIMED OUT after {MAX_WAIT}s without a real-call capture")
    snapshot("timeout")
    return 1


if __name__ == "__main__":
    sys.exit(main())
