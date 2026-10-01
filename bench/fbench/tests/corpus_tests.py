"""rf.p25_corpus: many-recording P25 replay, scored against SDRTrunk's decode.

Manifest-driven (``tools/p25_corpus_index.py`` -> ``bench/.state/corpus/manifest.json``).
Each item is one single-pass stream from the TX board (``fbench-agent replay
stream | iio_writedev``); the DUT runs p25-httpd. Scoring is per transmission
from p25-httpd's own per-call counts (``/api/ui/calls`` ``imbe``, exact per
call_id since 057) matched to the ``.mbe`` transmissions by TG, source and time.
The raw IMBE frames tapped from ``/api/imbe_dump`` (128-frame ring, polled every
``tap_period_s``) are only a bit-accuracy check: aligned in order with the
``.mbe`` frames (Hamming <= ``max_bits`` of 144), with the tap's coverage of the
extracted frames; they never change the recovery score. For focus
items (the 2026-05-03 two-tone alert) ``/ws/audio`` is recorded and the tone's
frequency stability and dropouts are measured.

Resumable (``-p resume=<run dir>|auto``) and stoppable: create
``bench/.state/corpus/STOP`` (checked every tap poll) or pass ``max_minutes``;
completed items are kept and analysed.
"""

from __future__ import annotations

import collections
import json
import time
import wave
from contextlib import ExitStack
from pathlib import Path
from typing import Any

import numpy as np

from .. import corpus as cp
from ..analysis import p25_corpus as pc
from ..analysis import p25_score as sc
from ..errors import (
    FbenchError,
    Inconclusive,
    PreconditionError,
    SafetyRefusal,
    TransportError,
    UsageError,
)
from ..runner import AnalysisContext, Outcome, TestContext, bench_test
from ..stimulus import check_transceiver, direction_metrics, reverse_direction_note

STOP_NAME = "STOP"


def corpus_dir(cfg: Any) -> Path:
    return Path(cfg.paths.state_dir) / "corpus"


def _manifest_path(ctx: Any) -> Path:
    p = str(ctx.params["manifest"])
    return Path(p) if p else corpus_dir(ctx.cfg) / "manifest.json"


def _load_manifest(ctx: Any) -> dict[str, Any]:
    path = _manifest_path(ctx)
    if not path.exists():
        raise PreconditionError(f"no corpus manifest at {path}: run "
                                "`python tools/p25_corpus_index.py` first")
    try:
        return pc.load_manifest(path)
    except ValueError as exc:
        raise PreconditionError(str(exc)) from exc


def _select(ctx: Any, man: dict[str, Any], mode: str) -> list[str]:
    ids = cp.item_ids(man, mode, str(ctx.params["a_unit"]))
    want = str(ctx.params["items"]).strip()
    if want and want != "all":
        if want == "focus":
            ids = [i for i in ids if _is_focus(man, mode, i)]
        else:
            # `-p items=a,b` reaches a str parameter as a JSON list (runner.coerce).
            sel = ([str(x) for x in json.loads(want)] if want.startswith("[")
                   else [x.strip() for x in want.split(",") if x.strip()])
            unknown = [x for x in sel if x not in ids]
            if unknown:
                raise UsageError(f"unknown mode {mode} items: {unknown}")
            ids = sel
    if int(ctx.params["limit"]) > 0:
        ids = ids[:int(ctx.params["limit"])]
    return ids


def _is_focus(man: dict[str, Any], mode: str, iid: str) -> bool:
    p = man["plans"]
    if mode == "B":
        return any(s["id"] == iid and s.get("focus") for s in p["B"]["scenes"])
    if mode == "C":
        items = {it["id"]: it for it in p["C"]["items"]}
        return any(b["id"] == iid and any(items[x]["focus"] for x in b["items"])
                   for b in p["C"]["batches"])
    return False


def _resume(ctx: TestContext, mode: str) -> dict[str, Path]:
    """Completed item files of a previous run (``resume=<run dir>`` or ``auto``)."""
    ref = str(ctx.params["resume"]).strip()
    if not ref:
        return {}
    if ref == "auto":
        runs = sorted(Path(ctx.cfg.paths.diagnostics_dir).glob("*/bench/run_*_rf.p25_corpus*"))
        runs = [r for r in runs if r != ctx.run_dir and (r / "params.json").exists()
                and json.loads((r / "params.json").read_text(encoding="utf-8")).get("mode")
                == mode]
        if not runs:
            ctx.warn("resume=auto: no earlier rf.p25_corpus run of this mode")
            return {}
        ref = str(runs[-1])
    d = Path(ref) / "artifacts" / "items"
    if not d.is_dir():
        raise UsageError(f"resume: {d} has no items")
    done = {}
    for f in sorted(d.glob("*.json")):
        try:
            doc = json.loads(f.read_text(encoding="utf-8"))
        except ValueError:
            continue
        if doc.get("status") == "done":
            done[doc["id"]] = f
    ctx.log.info("resume from %s: %d items done", ref, len(done))
    return done


def _trim(ctx: TestContext, tx: str, item: cp.Item) -> tuple[float, str]:
    p = ctx.params
    if float(p["tx_lo_offset_hz"]):
        return float(p["tx_lo_offset_hz"]), "tx_lo_offset_hz"
    if str(p["lo_trim"]) == "off":
        return 0.0, "off"
    ref_tx = ctx.cfg.unit(tx).ref_ppm
    if item.recorder == "true":
        if ref_tx is None:
            ctx.warn(f"units.{tx}.ref_ppm not set: TX LO not trimmed")
            return 0.0, "ref_ppm unknown"
        return -ref_tx * 1e-6 * item.centre_hz, f"reference-true content - {tx} {ref_tx:+.3f}"
    rec = item.recorder
    if rec not in ctx.cfg.units:
        ctx.warn(f"recorder {rec!r} is not a configured unit: TX LO not trimmed")
        return 0.0, "unknown recorder"
    ref_rec = ctx.cfg.unit(rec).ref_ppm
    if ref_rec is None or ref_tx is None:
        ctx.warn(f"units.{rec}.ref_ppm / units.{tx}.ref_ppm not set: TX LO not trimmed")
        return 0.0, "ref_ppm unknown"
    return (ref_rec - ref_tx) * 1e-6 * item.centre_hz, \
        f"ref_ppm {rec} {ref_rec:+.3f} - {tx} {ref_tx:+.3f}"


def _stop_requested(ctx: Any) -> bool:
    return (corpus_dir(ctx.cfg) / STOP_NAME).exists()


def _imbe_counters(http: Any) -> dict[str, Any] | None:
    try:
        tr = http.get_json("/api/traffic", None, 5.0) or {}
    except FbenchError:
        return None
    imbe = tr.get("imbe") or {}
    out = {k: imbe.get(k) for k in ("hdu_count", "ldu1_count", "ldu2_count", "tdu_count",
                                    "tdu_lc_count", "imbe_frames_extracted",
                                    "imbe_frames_dropped")}
    out["grants_seen_new"] = tr.get("grants_seen_new")  # CC grants decoded (delta per item)
    wd = tr.get("pll_watchdog") or {}
    out["pll_wd_resets_onset"] = wd.get("resets_onset")
    out["pll_wd_resets_pinned"] = wd.get("resets_pinned")
    # Control chain at the item boundary (after the inter-item silence): a PLL
    # pinned at its clamp here would explain a late control-channel acquisition.
    cc = tr.get("control_lsm_agc")
    if isinstance(cc, dict):
        out["control"] = {k: cc.get(k) for k in ("pll_dbg", "agc_gain", "agc_mag",
                                                  "mag_update_threshold")}
    return out


# ---------------------------------------------------------------------------
# Mode C: park the traffic chain on the batch channel
# ---------------------------------------------------------------------------


def _traffic_state(http: Any) -> dict[str, Any]:
    d = http.get_json("/api/traffic", None, 5.0) or {}
    return {"follower_enabled": d.get("follower_enabled"), "lock_freq": d.get("lock_freq"),
            "freq_hz": d.get("current_frequency_hz"), "tg": d.get("current_talkgroup")}


def _primer_lock(ctx: TestContext, http: Any, item: cp.Item, taps: cp.Taps,
                 t_stream0: float) -> dict[str, Any]:
    """Follower on and unlocked until the primer grant parks it on the channel, then
    ``lock=on&follower=off`` (p25-httpd's air-time gate feeds dibits only under a
    talkgroup context, which a manual retune alone does not set)."""
    pr = item.primer or {}
    chan = float(pr["channel_hz"])
    http.get_json("/api/traffic", {"follower": "on", "lock": "off"}, 5.0)
    deadline = t_stream0 + float(pr.get("grant_stream_s", 4.0)) + float(ctx.params["primer_wait_s"])
    polls = []
    while ctx.services.monotonic() < deadline:
        s = _traffic_state(http)
        polls.append({"t": round(ctx.services.monotonic() - t_stream0, 2), **s})
        if s["freq_hz"] and abs(float(s["freq_hz"]) - chan) < 1000 and s["tg"]:
            http.get_json("/api/traffic", {"lock": "on", "follower": "off"}, 5.0)
            after = _traffic_state(http)
            ok = bool(after["lock_freq"]) and after["follower_enabled"] is False and \
                after["freq_hz"] is not None and abs(float(after["freq_hz"]) - chan) < 1000
            return {"locked": ok, "at_stream_s": polls[-1]["t"], "tg": s["tg"],
                    "state": after, "polls": polls}
        taps.poll(calls=False)
        ctx.sleep(0.5)
    return {"locked": False, "polls": polls}


# ---------------------------------------------------------------------------
# Acquisition
# ---------------------------------------------------------------------------


def _save_audio(ctx: TestContext, name: str, rec: dict[str, Any], mono_minus_wall: float
                ) -> dict[str, Any]:
    from ..wsaudio import lag_events

    chunks = rec.get("chunks") or []
    lanes = []
    # One WAV per lane; the first lane's keeps the old name and the top-level keys.
    for lane in sorted({c[1] for c in chunks}) or [0]:
        mine = [c for c in chunks if c[1] == lane]
        file = f"items/{name}_audio.wav" if not lanes else f"items/{name}_audio_lane{lane}.wav"
        with wave.open(str(ctx.artifact_path(file)), "wb") as wf:
            wf.setnchannels(1)
            wf.setsampwidth(2)
            wf.setframerate(8000)
            wf.writeframes(b"".join(c[2] for c in mine))
        sizes = sorted({len(c[2]) for c in mine})
        lanes.append({"lane": lane, "file": file, "chunks": len(mine),
                      "t": [round(c[0] + mono_minus_wall, 4) for c in mine], "sizes": sizes,
                      "lens": [len(c[2]) for c in mine] if sizes not in ([], [320]) else None})
    return {**lanes[0], "lanes": lanes,
            "lags": [{"t": round(x["t"] + mono_minus_wall, 3), "skipped": x["skipped"]}
                     for x in lag_events(rec.get("texts") or [])],
            "error": rec.get("error")}


def _run_item(ctx: TestContext, man: dict[str, Any], item: cp.Item, stager: cp.Stager,
              relay: cp.Relay, tx: str, rx: str) -> dict[str, Any]:
    p = ctx.params
    http = ctx.http(rx)
    phy = ctx.cfg.iio.phy_device
    doc: dict[str, Any] = {"id": item.id, "mode": item.mode, "status": "error", "error": None,
                           "spec": item.spec(), "started": time.strftime("%Y-%m-%dT%H:%M:%S")}
    t_run0 = ctx.services.monotonic()
    ring = int(p["ring_mb"]) if p["source"] == "sd" else min(int(p["ring_mb"]), 32)
    if p["source"] == "ram":
        # RAM holds one item at a time: drop the previous item's files first.
        doc["ram_purged"] = stager.purge({f.name for f in item.files})
    doc["staging"] = stager.ensure(item.files, reserve=(ring << 20) if p["source"] == "ram" else 0)
    check_transceiver(ctx, tx, item.centre_hz, item.rate_hz)
    trim, why = _trim(ctx, tx, item)
    tx_lo = int(round(item.centre_hz + trim))
    bw = float(p["tx_rf_bandwidth_hz"]) or min(max(1.25 * item.rate_hz, 200e3),
                                               ctx.cfg.unit(tx).limits["rf_bw_max_hz"])
    doc["tx"] = {"lo_hz": tx_lo, "trim_hz": round(trim, 1), "trim_from": why,
                 "rate_hz": item.rate_hz, "rf_bandwidth_hz": bw,
                 "atten_db": float(p["tx_atten_db"])}
    # Rule 3: attenuation first, then LO / rate / bandwidth, then the source.
    ctx.iio_set(tx, phy, "hardwaregain", f"{-float(p['tx_atten_db']):g}", "voltage0", True,
                tx_ok=True)
    ctx.iio_set(tx, phy, "frequency", f"{tx_lo}", "altvoltage1", True)
    ctx.iio_set(tx, phy, "sampling_frequency", f"{int(item.rate_hz)}", "voltage0", True)
    ctx.iio_set(tx, phy, "rf_bandwidth", f"{int(bw)}", "voltage0", True)
    ctx.mark_tx_active(tx)
    taps = cp.Taps(ctx, rx, calls_every=max(1, int(float(p["calls_period_s"]) /
                                                   max(float(p["tap_period_s"]), 0.1))))
    doc["dut_clock"] = taps.dut_clock()
    doc["counters_before"] = _imbe_counters(http)
    audio = None
    wall0, mono0 = time.time(), ctx.services.monotonic()
    if p["ws_audio"]:
        try:
            audio = ctx.services.ws_audio(rx)
        except (OSError, FbenchError, AttributeError) as exc:
            ctx.warn(f"/ws/audio not recorded: {exc}")
    stopped = False
    baseline = taps.baseline()
    try:
        relay.start(item)
        # Prefill: the ring's lead over the air absorbs SD stalls.
        deadline = ctx.services.monotonic() + float(p["prefill_timeout_s"])
        st0 = None
        while ctx.services.monotonic() < deadline:
            st0 = relay.status()
            if st0 and st0.get("state") in ("streaming", "underrun", "done", "error", "stopped"):
                break
            ctx.sleep(1.0)
        if not st0 or st0.get("state") not in ("streaming", "underrun", "done"):
            raise FbenchError(f"relay on {tx} did not start streaming: {st0}")
        m_read = ctx.services.monotonic()
        t_first = st0.get("t_first_out_unix")
        stream0 = m_read - (float(st0["now_unix"]) - float(t_first)) if t_first else m_read
        doc["stream0_mono"] = round(stream0, 3)
        doc["prefill_s"] = st0.get("prefill_s")
        if item.mode == "C":
            doc["lock"] = _primer_lock(ctx, http, item, taps, stream0)
            if not doc["lock"]["locked"]:
                raise FbenchError(f"mode C primer: the follower did not park on "
                                  f"{item.primer['channel_hz'] / 1e6:.4f} MHz "
                                  f"(TG {item.primer['grant']['tg']}); see lock.polls")
        end = stream0 + item.seconds + float(p["tail_s"])
        hard = stream0 + item.seconds * 1.5 + float(p["prefill_timeout_s"]) + 60
        k, last = 0, st0
        while True:
            now = ctx.services.monotonic()
            if _stop_requested(ctx):
                stopped = True
                break
            if now >= end and (last or {}).get("state") in ("done", "error", "stopped",
                                                             "downstream_closed"):
                break
            if now >= hard:
                ctx.warn(f"{item.id}: relay still {(last or {}).get('state')} after "
                         f"{now - stream0:.0f} s; stopping it")
                break
            ctx.sleep(float(p["tap_period_s"]))
            taps.poll()
            k += 1
            if k % 10 == 0 or ctx.services.monotonic() >= end:
                last = relay.status() or last
    finally:
        relay.stop()
        rec = audio.stop() if audio is not None else None
    doc["relay"] = relay.report() or relay.status()
    doc["dut_clock_end"] = taps.dut_clock()  # the DUT clock can be set mid-item
    taps.poll_calls()
    doc["counters_after"] = _imbe_counters(http)
    merged = sc.merge_dumps(taps.dumps, skip_first=taps.has_baseline)
    doc["tap"] = {"polls": len(taps.dumps), "gaps": merged["gaps"],
                  "ambiguous": merged["ambiguous"], "errors": taps.errors[:20],
                  "period_s": float(p["tap_period_s"]), "ring": sc.RING,
                  "frames": merged["frames"]}
    if taps.has_baseline:
        doc["tap"]["baseline_frames"] = baseline
    calls = sc.merge_calls(taps.calls)
    clk = doc.get("dut_clock") or {}
    if clk.get("dut_minus_mono_s") is not None:
        # /api/ui/calls lists earlier items' calls too (and, with the DUT clock
        # unset, calls stamped before a reboot sit later): keep this item's.
        t_min = (float(clk["dut_minus_mono_s"]) + t_run0 - 5.0) * 1000.0
        t_max = (float(clk["dut_minus_mono_s"]) + ctx.services.monotonic() + 5.0) * 1000.0
        calls = [c for c in calls if t_min <= float(c.get("started_unix_ms") or 0) <= t_max]
    doc["calls"] = calls
    if rec is not None:
        doc["audio"] = _save_audio(ctx, item.id, rec, mono0 - wall0)
    doc["truth"] = {k: [[round(s, 3), h] for s, h in v]
                    for k, v in cp.truth_stream_frames(item, man).items()}
    doc["elapsed_s"] = round(ctx.services.monotonic() - t_run0, 1)
    doc["status"] = "stopped" if stopped else "done"
    return doc


def rf_p25_corpus(ctx: TestContext) -> Outcome:
    from ..runner import maintenance

    p = ctx.params
    mode = str(p["mode"]).upper()
    if mode not in ("A", "B", "C"):
        raise UsageError("mode must be A, B or C")
    if p["source"] not in ("sd", "ram"):
        raise UsageError("source must be sd or ram")
    tx, rx = ctx.roles["tx"], ctx.roles["rx"]
    man = _load_manifest(ctx)
    ids = _select(ctx, man, mode)
    if not ids:
        raise PreconditionError(f"no mode {mode} items selected")
    done = _resume(ctx, mode)
    todo = [i for i in ids if i not in done]
    for iid, f in done.items():
        if iid in ids:
            ctx.artifact_path(f"items/{iid}.json").write_bytes(f.read_bytes())
            for wav in f.parent.glob(f"{iid}_audio*.wav"):
                ctx.artifact_path(f"items/{wav.name}").write_bytes(wav.read_bytes())
    ctx.require_agent(tx)
    tx_image = ctx.caps(tx)["image"]
    if tx_image not in ("p25", "hwval"):
        raise PreconditionError(f"{tx} runs image {tx_image!r}: the relay needs a Tezuka image "
                                "(fbench-agent + iio_writedev)")
    root = cp.CORPUS_SD if p["source"] == "sd" else cp.CORPUS_RAM
    stager = cp.Stager(ctx, tx, root, corpus_dir(ctx.cfg) / f"staged_{tx}.json")
    run: dict[str, Any] = {"mode": mode, "manifest": str(_manifest_path(ctx)),
                           "manifest_generated": man.get("generated"), "items": ids,
                           "resumed": sorted(set(done) & set(ids)), "source": p["source"],
                           "stopped": False, "errors": {}}
    if p["stage_only"]:
        staged = []
        keep: set[str] = set()
        for iid in todo:
            item = cp.build_item(man, mode, iid, a_unit=str(p["a_unit"]),
                                 a_recorder=ctx.cfg.rf.p25_clip_recorder or "A",
                                 noise_db=float(p["noise_db"]))
            keep |= {f.name for f in item.files}
            if _stop_requested(ctx):
                run["stopped"] = True
                break
            staged += stager.ensure(item.files)
        if p["purge"]:
            run["purged"] = stager.purge(keep)
        run["staged"] = staged
        ctx.save_json("corpus_run.json", run)
        up = [s for s in staged if s["uploaded"]]
        return ctx.outcome(f"mode {mode}: {len(staged)} files ready on {tx}:{root} "
                           f"({sum(s['bytes'] for s in staged) / 1e9:.2f} GB), {len(up)} uploaded "
                           f"({sum(s['bytes'] for s in up) / 1e9:.2f} GB)", "pass")
    ctx.require_image(rx, "p25")
    direction_metrics(ctx)
    reverse_direction_note(ctx)
    http = ctx.http(rx)
    run["build"] = (http.get_json("/api/system", None, 5.0) or {}).get("build")
    budget = float(p["max_minutes"]) * 60.0
    t0 = ctx.services.monotonic()
    prefill = None if int(p["prefill_mb"]) < 0 else int(p["prefill_mb"])
    relay = cp.Relay(ctx, tx, root, block_samples=int(p["block_samples"]),
                     ring_mb=int(p["ring_mb"]) if p["source"] == "sd" else min(int(p["ring_mb"]), 32),
                     prefill_mb=prefill, on_underrun=str(p["on_underrun"]))
    orig_traffic = None
    # p25-httpd 067: with the clock source "site" the DUT sets its clock
    # from the replayed control channel (a different day per item). The
    # scoring needs a steady DUT clock, so pin it to "manual" for the run.
    orig_clock = None
    try:
        doc = http.get_json("/api/ui/settings", None, 5.0) or {}
        src = ((doc.get("settings") or {}).get("clock") or {}).get("source")
        if src and src != "manual":
            http.put_json("/api/ui/settings", {"clock": {"source": "manual"}}, 5.0)
            orig_clock = src
            run["clock_source_before"] = src
    except FbenchError as exc:
        ctx.warn(f"could not pin the DUT clock source: {exc.message}")
    with ExitStack() as stack:
        if tx_image == "p25":
            stack.enter_context(maintenance(ctx, tx))  # rule 4: TX rate changes corrupt its RX
        if mode == "C":
            orig_traffic = _traffic_state(http)
            run["traffic_before"] = orig_traffic
            mon = http.get_json("/api/monitor", None, 5.0) or {}
            tgs = mon.get("talkgroups") or []
            pr_tg = ((man["plans"]["C"].get("primer") or {}).get("grant") or {}).get("tg")
            if tgs and pr_tg not in tgs:
                raise PreconditionError(f"/api/monitor lists {tgs} without the primer TG {pr_tg}: "
                                        "the follower would ignore the primer grant")
        try:
            for n, iid in enumerate(todo):
                if _stop_requested(ctx):
                    run["stopped"] = True
                    break
                if budget and ctx.services.monotonic() - t0 > budget:
                    run["stopped"] = True
                    ctx.warn(f"max_minutes reached after {n} items")
                    break
                item = cp.build_item(man, mode, iid, a_unit=str(p["a_unit"]),
                                     a_recorder=ctx.cfg.rf.p25_clip_recorder or "A",
                                     noise_db=float(p["noise_db"]))
                ctx.log.info("item %d/%d %s: %.1f s at %.3f MHz, %d transmissions", n + 1,
                             len(todo), iid, item.seconds, item.centre_hz / 1e6,
                             len(item.transmissions))
                try:
                    doc = _run_item(ctx, man, item, stager, relay, tx, rx)
                except (SafetyRefusal, TransportError):
                    raise
                except FbenchError as exc:
                    run["errors"][iid] = exc.message
                    ctx.errors.append(f"{iid}: {exc.message}")
                    ctx.save_json(f"items/{iid}.json", {"id": iid, "mode": mode,
                                                        "status": "error",
                                                        "error": exc.message,
                                                        "spec": item.spec()})
                    continue
                ctx.save_json(f"items/{iid}.json", doc)
                if doc["status"] == "stopped":
                    run["stopped"] = True
                    break
        finally:
            relay.stop()
            try:
                ctx.tx_off(tx)  # before maintenance exit restarts p25-httpd on the TX board
            except FbenchError as exc:
                ctx.errors.append(f"tx off on {tx}: {exc.message}")
            if orig_traffic is not None and p["restore_traffic"]:
                try:
                    http.get_json("/api/traffic", {
                        "follower": "on" if orig_traffic["follower_enabled"] is not False
                        else "off", "lock": "on" if orig_traffic["lock_freq"] else "off"}, 5.0)
                    run["traffic_after"] = _traffic_state(http)
                except FbenchError as exc:
                    ctx.errors.append(f"could not restore /api/traffic follower/lock: "
                                      f"{exc.message}")
            if orig_clock is not None:
                try:
                    http.put_json("/api/ui/settings", {"clock": {"source": orig_clock}}, 5.0)
                except FbenchError as exc:
                    ctx.errors.append(f"could not restore the DUT clock source {orig_clock}: "
                                      f"{exc.message}")
            if _stop_requested(ctx):
                (corpus_dir(ctx.cfg) / STOP_NAME).unlink(missing_ok=True)
    run["elapsed_s"] = round(ctx.services.monotonic() - t0, 1)
    ctx.save_json("corpus_run.json", run)
    return analyze_corpus(ctx)


# ---------------------------------------------------------------------------
# Analysis
# ---------------------------------------------------------------------------


def _tap_new_frames(doc: dict[str, Any]) -> tuple[list[dict[str, Any]], int]:
    """Tapped frames decoded during the item: the first dump is only a reference
    (``baseline_frames``); runs before that field drop what exceeds the
    ``imbe_frames_extracted`` delta (stale ring content)."""
    tap = doc.get("tap") or {}
    frames = tap.get("frames") or []
    if "baseline_frames" in tap:
        return frames, 0
    delta = (_counter_delta(doc) or {}).get("imbe_frames_extracted")
    stale = max(0, len(frames) - int(delta)) if delta is not None else 0
    return frames[stale:], stale


def _item_calls(doc: dict[str, Any], clock_end: float | None = None) -> list[dict[str, Any]]:
    """The item's own calls: those that started while its stream played, on the
    DUT clock sampled at the item (A's clock can be unset after a reboot and jump
    to real time between items, so earlier calls may sit decades either side).

    ``clock_end``: DUT-minus-host clock after the item (``dut_clock_end``, else the
    next item's). When it differs, the DUT clock was set during the item (bench
    2026-09-27, B_20260502_093344_13: from uptime to real time at ~110 s); calls
    stamped after the step are moved back onto the item's clock."""
    calls = [c for c in (doc.get("calls") or []) if c.get("started_unix_ms")]
    clk = (doc.get("dut_clock") or {}).get("dut_minus_mono_s")
    s0 = doc.get("stream0_mono")
    if clk is None or s0 is None:
        return calls
    t0 = float(clk) + float(s0)
    span = float((doc.get("spec") or {}).get("seconds") or doc.get("elapsed_s") or 600.0)
    lo, hi = (t0 - 30.0) * 1000.0, (t0 + span + 60.0) * 1000.0
    out = [c for c in calls if lo <= float(c["started_unix_ms"]) <= hi]
    if clock_end is None:
        clock_end = (doc.get("dut_clock_end") or {}).get("dut_minus_mono_s")
    if clock_end is not None and abs(float(clock_end) - float(clk)) > 60.0:
        step = (float(clock_end) - float(clk)) * 1000.0
        for c in calls:
            if lo + step <= float(c["started_unix_ms"]) <= hi + step:
                moved = {**c, "started_unix_ms": float(c["started_unix_ms"]) - step,
                         "clock_step_s": round(step / 1000.0, 3)}
                if c.get("ended_unix_ms"):
                    moved["ended_unix_ms"] = float(c["ended_unix_ms"]) - step
                out.append(moved)
        out.sort(key=lambda c: float(c["started_unix_ms"]))
    return out


def _busy_spans(man: dict[str, Any] | None, doc: dict[str, Any]
                ) -> list[tuple[float, float, float]]:
    """SDRTrunk's traffic channel recordings on the item's stream clock,
    ``(freq_hz, s0, s1)``: when its channels were allocated (grant to teardown)."""
    tl = (doc.get("spec") or {}).get("timeline") or []
    out = []
    for r in (man or {}).get("recordings") or []:
        if r.get("kind") != "traffic" or r.get("start_unix") is None:
            continue
        a0, a1 = float(r["start_unix"]), float(r["start_unix"]) + float(r.get("seconds") or 0)
        for seg in tl:
            e0, e1 = seg["air0"], seg["air0"] + seg["s1"] - seg["s0"]
            if a0 < e1 and e0 < a1:
                out.append((float(r["freq_hz"]), seg["s0"] + a0 - seg["air0"],
                            seg["s0"] + a1 - seg["air0"]))
    return out


def score_item(doc: dict[str, Any], txs: dict[str, dict[str, Any]], max_bits: int = 24,
               busy: list[tuple[float, float, float]] | None = None,
               clock_end: float | None = None) -> dict[str, Any]:
    """Per transmission: recovered frames from p25-httpd's own per-call ``imbe``
    (primary); the hex tap adds a bit-accuracy check that never lowers it.

    ``busy``: SDRTrunk's channel allocations (:func:`_busy_spans`) for
    :func:`p25_score.blockers`; ``clock_end``: see :func:`_item_calls`."""
    truth = {k: [(float(s), str(h)) for s, h in v] for k, v in (doc.get("truth") or {}).items()}
    stream_txs = []
    for tid, fr in truth.items():
        tx = txs.get(tid)
        if tx is None or not fr:
            continue
        stream_txs.append({**tx, "frames": len(fr), "t0": fr[0][0], "t1": fr[-1][0] + 0.02})
    stream_txs.sort(key=lambda t: t["t0"])
    calls = _item_calls(doc, clock_end)
    clk = (doc.get("dut_clock") or {}).get("dut_minus_mono_s")
    prior = float(clk) + float(doc["stream0_mono"]) if clk is not None and \
        doc.get("stream0_mono") is not None else None
    by_calls = sc.score_by_calls(stream_txs, calls, prior=prior)
    # Secondary: raw codewords from the /api/imbe_dump tap, aligned in order.
    tapped, stale = _tap_new_frames(doc)
    order = [(t["id"], f) for t in stream_txs for f in truth[t["id"]]]
    al = sc.align_bits([f for _, f in order], tapped, max_bits=max_bits)
    per_tx_aligned: dict[str, int] = {}
    for i in al["matched_idx"]:
        per_tx_aligned[order[i][0]] = per_tx_aligned.get(order[i][0], 0) + 1
    delta = (_counter_delta(doc) or {}).get("imbe_frames_extracted")
    rows = []
    for t in stream_txs:
        r = by_calls["rows"].get(t["id"], {})
        rec = int(r.get("recovered") or 0)
        rows.append({"id": t["id"], "item": doc["id"], "call": t["call"], "tg": t["tg"],
                     "src": t["src"], "freq_hz": t["freq_hz"], "encrypted": t["encrypted"],
                     "stream_s": round(t["t0"], 3), "seconds": round(t["t1"] - t["t0"], 2),
                     "truth": t["frames"], "recovered": rec,
                     "recovery_pct": round(100.0 * rec / t["frames"], 1),
                     "dut_call_id": r.get("call_id"), "dut_call_ids": r.get("call_ids") or [],
                     "dut_match": r.get("match"), "dut_followed": bool(r.get("followed")),
                     "dut_ldu": r.get("ldu"),
                     "hex_aligned": per_tx_aligned.get(t["id"], 0)})
    mixed = doc.get("mode") in ("A", "B")
    conf = sc.conflicts(stream_txs) if mixed else {}
    blk = sc.blockers(stream_txs, {r["id"]: r["dut_followed"] for r in rows},
                      busy or ()) if mixed else {}
    for r in rows:
        r["conflicts"] = conf.get(r["id"], [])
        r["blocked_by"] = blk.get(r["id"], [])
        # One traffic chain, first come first served: not followable when the chain
        # was legitimately busy with another followed call at this one's grant.
        r["followable"] = not r["encrypted"] and not (
            r["blocked_by"] and (r["recovery_pct"] or 0) < 50)
    relay = doc.get("relay") or {}
    tap = doc.get("tap") or {}
    return {"id": doc["id"], "status": doc.get("status"), "error": doc.get("error"),
            "dut_offset_s": by_calls["offset_s"], "rows": rows,
            "calls": {"calls": by_calls["calls"], "unmatched_calls": by_calls["unmatched_calls"],
                      "close_reasons": dict(collections.Counter(
                          str(c.get("close_reason")) for c in calls))},
            "bits": {"tapped": len(tapped), "stale_dropped": stale, "extracted": delta,
                     "coverage_pct": round(100.0 * len(tapped) / delta, 1) if delta else None,
                     "aligned": al["aligned"], "exact": al["exact"],
                     "mean_bit_diff": al["mean_bit_diff"], "truth": al["truth"],
                     "hist": dict(collections.Counter(min(b, 24) for b in al["bits"]))},
            "relay": {k: relay.get(k) for k in ("state", "complete", "underruns",
                                                "underrun_ms", "ring_min_fill_s",
                                                "read_max_ms", "read_mbs", "zero_samples",
                                                "prefill_s", "samples_out")},
            "tap": {k: tap.get(k) for k in ("polls", "gaps", "ambiguous", "period_s")},
            "counters": _counter_delta(doc)}


def _counter_delta(doc: dict[str, Any]) -> dict[str, Any] | None:
    a, b = doc.get("counters_before"), doc.get("counters_after")
    if not a or not b:
        return None
    return {k: (b.get(k) or 0) - (a.get(k) or 0) for k in b if isinstance(b.get(k), (int, float))}


def _audio_offset(doc: dict[str, Any], it: dict[str, Any]) -> float | None:
    """Host-monotonic minus stream seconds, for the /ws/audio chunk times: from the
    call-matched DUT clock offset, else from the relay's first-sample time."""
    clk = (doc.get("dut_clock") or {}).get("dut_minus_mono_s")
    if it.get("dut_offset_s") is not None and clk is not None:
        return float(it["dut_offset_s"]) - float(clk)
    if doc.get("stream0_mono") is not None:
        return float(doc["stream0_mono"]) + 0.8
    return None


def _tone(a: AnalysisContext, doc: dict[str, Any], man_ref: dict[str, Any], rows: list[dict],
          offset: float | None) -> dict[str, Any] | None:
    audio = doc.get("audio")
    if not audio or not man_ref or offset is None:
        return None
    refs = [r for c in man_ref.values() for r in c.get("mp3", [])
            if r.get("frames") and r["tonal_frames"] / r["frames"] >= 0.5 and r["tones_hz"]]
    if not refs:
        return None
    ref = refs[0]
    calls = set(man_ref)
    txr = sorted((r for r in rows if r["call"] in calls), key=lambda r: r["stream_s"])
    if not txr:
        return None
    first = txr[0]
    lo = first["stream_s"] + offset - 2.0
    hi = first["stream_s"] + first["seconds"] + offset + 3.0
    # The focus call plays on one lane: score each and keep the one with the tone.
    res = None
    for lane in audio.get("lanes") or [audio]:
        path = a.artifacts_dir / lane["file"]
        if not path.exists():
            continue
        with wave.open(str(path), "rb") as wf:
            pcm = np.frombuffer(wf.readframes(wf.getnframes()), dtype="<i2")
        times = lane["t"]
        chunks, pos = [], 0
        for t, sz in zip(times, lane.get("lens") or [320] * len(times)):
            n = sz // 2
            if lo <= t <= hi:
                chunks.append({"t": t, "pcm": pcm[pos:pos + n]})
            pos += n
        r = sc.tone_continuity(chunks, ref["tones_hz"])
        r["lane"] = lane.get("lane", 0)
        if res is None or (r.get("found"), r.get("tone_frames", 0)) > (res.get("found"),
                                                                       res.get("tone_frames", 0)):
            res = r
    if res is None:
        return None
    res.update(reference_tones_hz=ref["tones_hz"], reference_mp3=ref.get("mp3"),
               reference_tonal_frames=ref["tonal_frames"], transmission=first["id"],
               lags_in_window=sum(1 for x in audio.get("lags", []) if lo <= x["t"] <= hi))
    return res


def analyze_corpus(a: AnalysisContext) -> Outcome:
    run = a.load_json("corpus_run.json")
    items_dir = a.artifacts_dir / "items"
    docs = []
    for f in sorted(items_dir.glob("*.json")) if items_dir.is_dir() else []:
        docs.append(a.load_json(f"items/{f.name}"))
    mode = run.get("mode")
    a.metric("mode", mode)
    a.metric("items_selected", len(run.get("items", [])))
    a.metric("items_run", sum(1 for d in docs if d.get("status") == "done"))
    a.metric("items_error", sum(1 for d in docs if d.get("status") == "error"))
    a.metric("stopped", bool(run.get("stopped")))
    if run.get("staged") is not None:
        return a.outcome(f"staging only: {len(run['staged'])} files", "pass")
    try:
        man = pc.load_manifest(run["manifest"])
        txs = pc.tx_index(man)
        focus_ref = man.get("focus_reference") or {}
    except (OSError, ValueError, KeyError):
        man, focus_ref = None, {}
        txs = {}
    if not txs:
        # Offline re-analysis without the manifest: rebuild minimal records from the items.
        for d in docs:
            for tid, fr in (d.get("truth") or {}).items():
                txs.setdefault(tid, {"id": tid, "call": tid.split("#")[0], "tg": None,
                                     "src": None, "freq_hz": None, "encrypted": False,
                                     "frames": len(fr)})
    max_bits = int(a.params.get("max_bits", 24))
    # DUT clock after each item: its own end sample, else the next item's start sample.
    order = {iid: k for k, iid in enumerate(run.get("items") or [])}
    seq = sorted(docs, key=lambda d: (order.get(d.get("id"), len(order)), d.get("started") or ""))
    clock_end = {}
    for d, nxt in zip(seq, seq[1:] + [None]):
        end = (d.get("dut_clock_end") or (nxt or {}).get("dut_clock") or {})
        clock_end[d.get("id")] = end.get("dut_minus_mono_s")
    items = [score_item(d, txs, max_bits, busy=_busy_spans(man, d), clock_end=clock_end[d["id"]])
             for d in docs if d.get("status") in ("done", "stopped")]
    rows = [r for it in items for r in it["rows"]]
    tone_res = []
    for d, it in zip([d for d in docs if d.get("status") in ("done", "stopped")], items):
        if (d.get("spec") or {}).get("focus"):
            t = _tone(a, d, focus_ref, it["rows"], _audio_offset(d, it))
            if t is not None:
                t["item"] = d["id"]
                tone_res.append(t)
    a.save_json("scores.json", {"items": items, "tone": tone_res})
    if not rows:
        errs = run.get("errors") or {}
        raise Inconclusive(f"no scored transmissions ({len(docs)} item files, "
                           f"{len(errs)} errors: {list(errs.items())[:3]})")
    clear = [r for r in rows if not r["encrypted"]]
    foll = [r for r in clear if r.get("followable", True)]
    tf = sum(r["truth"] for r in foll)
    rf = sum(r["recovered"] for r in foll)
    a.metric("transmissions", len(rows))
    a.metric("transmissions_clear", len(clear))
    a.metric("transmissions_followable", len(foll))
    a.metric("truth_frames_followable", tf)
    a.metric("recovered_frames", rf)
    a.metric("score_source", "p25-httpd per-call imbe (/api/ui/calls)")
    pct = 100.0 * rf / tf if tf else None
    a.metric("clear_recovery_pct", round(pct, 2) if pct is not None else None,
             min=float(a.params.get("min_recovery_pct", 90.0)))
    raw_tf = sum(r["truth"] for r in clear)
    a.metric("clear_recovery_pct_all", round(100.0 * sum(r["recovered"] for r in clear) /
                                             raw_tf, 2) if raw_tf else None)
    missed = [r for r in foll if (r["recovery_pct"] or 0) < 10]
    a.metric("missed_transmissions", len(missed))
    a.metric("missed", [{"id": r["id"], "item": r["item"], "tg": r["tg"], "truth": r["truth"],
                         "recovered": r["recovered"]} for r in missed[:25]])
    part = [r for r in foll if 10 <= (r["recovery_pct"] or 0) < 90]
    a.metric("partial_transmissions", len(part))
    a.metric("encrypted_transmissions", len(rows) - len(clear))
    a.metric("conflicted_transmissions", sum(1 for r in clear if r.get("conflicts")))
    # Secondary: bit accuracy of the tapped raw codewords (never changes the score).
    tapped = sum(it["bits"]["tapped"] for it in items)
    ext = [it["bits"]["extracted"] for it in items if it["bits"]["extracted"] is not None]
    a.metric("tap_frames", tapped)
    a.metric("tap_coverage_pct", round(100.0 * tapped / sum(ext), 1) if ext and sum(ext) else None)
    al = sum(it["bits"]["aligned"] for it in items)
    a.metric("hex_aligned_frames", al)
    a.metric("hex_aligned_pct_of_truth", round(100.0 * al / max(1, sum(it["bits"]["truth"]
                                                                        for it in items)), 1))
    a.metric("hex_exact_pct_of_aligned", round(100.0 * sum(it["bits"]["exact"] for it in items)
                                               / al, 1) if al else None)
    mb = [(it["bits"]["mean_bit_diff"], it["bits"]["aligned"]) for it in items
          if it["bits"]["mean_bit_diff"] is not None]
    a.metric("hex_mean_bit_diff", round(sum(m * n for m, n in mb) / max(1, sum(n for _, n in mb)),
                                        3) if mb else None)
    per_item = []
    for it in items:
        fr = [r for r in it["rows"] if not r["encrypted"] and r.get("followable", True)]
        t_ = sum(r["truth"] for r in fr)
        per_item.append({"id": it["id"], "transmissions": len(fr), "truth": t_,
                         "recovered": sum(r["recovered"] for r in fr),
                         "recovery_pct": round(100.0 * sum(r["recovered"] for r in fr) / t_, 1)
                         if t_ else None,
                         "underruns": (it["relay"] or {}).get("underruns")})
    worst = sorted((x for x in per_item if x["recovery_pct"] is not None),
                   key=lambda x: x["recovery_pct"])[:10]
    a.metric("worst_items", worst)
    und = sum(int((it["relay"] or {}).get("underruns") or 0) for it in items)
    a.metric("relay_underruns", und, max=float(a.params.get("max_underruns", 0)))
    fills = [(it["relay"] or {}).get("ring_min_fill_s") for it in items]
    fills = [f for f in fills if f is not None]
    a.metric("relay_ring_min_fill_s", min(fills) if fills else None)
    reads = [(it["relay"] or {}).get("read_max_ms") for it in items]
    reads = [r for r in reads if r is not None]
    a.metric("relay_read_max_ms", max(reads) if reads else None)
    a.metric("relay_incomplete_items", sum(1 for it in items if (it["relay"] or {}).get(
        "complete") is False and it["status"] == "done"))
    closes: dict[str, int] = {}
    for it in items:
        for k, v in ((it.get("calls") or {}).get("close_reasons") or {}).items():
            closes[k] = closes.get(k, 0) + v
    a.metric("call_close_reasons", closes)
    a.metric("tap_gaps", sum(int((it["tap"] or {}).get("gaps") or 0) for it in items))
    tail = ""
    if tone_res:
        t = tone_res[0]
        a.metric("tone_found", bool(t.get("found")))
        if t.get("found"):
            a.metric("tone_dropouts", t["dropouts"],
                     max=float(a.params.get("max_tone_dropouts", 0)))
            a.metric("tone_frames", t["tone_frames"])
            a.metric("tone_per_tone", t["per_tone"])
            a.metric("tone_max_arrival_gap_ms", t.get("max_arrival_gap_ms"))
            tail = (f"; focus tone {t['tone_s']:.2f} s, {t['dropouts']} dropouts, "
                    + ", ".join(f"{k} Hz sd {v['std_hz']:.1f}" for k, v in
                                t["per_tone"].items()))
        else:
            a.warn("focus item ran but its tone was not found in /ws/audio")
            tail = "; focus tone NOT found in /ws/audio"
    if run.get("stopped"):
        a.warn(f"run stopped after {sum(1 for d in docs if d.get('status') == 'done')} of "
               f"{len(run.get('items', []))} items: resume with -p resume=<this run dir>")
    return a.outcome(
        f"mode {mode}: {len(items)} items, {len(foll)} followable clear transmissions, "
        f"{rf}/{tf} IMBE frames ({pct:.1f} %) of SDRTrunk's, {len(missed)} missed, "
        f"{und} relay underruns" + tail if pct is not None else f"mode {mode}: nothing scorable")


bench_test(
    "rf.p25_corpus", tier=0, units="tx,rx", tx=True,
    params={"mode": "A", "manifest": "", "items": "all", "limit": 0, "max_minutes": 0.0,
            "a_unit": "whole", "source": "sd", "stage_only": False, "purge": False,
            "resume": "", "tx_atten_db": 55.0, "lo_trim": "auto", "tx_lo_offset_hz": 0.0,
            "tx_rf_bandwidth_hz": 0.0, "ring_mb": 192, "prefill_mb": -1,
            "block_samples": 262144, "on_underrun": "wait", "prefill_timeout_s": 90.0,
            "tail_s": 6.0, "tap_period_s": 0.5, "calls_period_s": 15.0, "ws_audio": True,
            "primer_wait_s": 20.0, "noise_db": 35.0, "restore_traffic": True,
            "min_recovery_pct": 90.0, "max_underruns": 0, "max_tone_dropouts": 0,
            "max_bits": 24, "tx_port": ""},
    description="Replay many P25 recordings from the TX board and score the DUT per "
                "transmission against SDRTrunk's .mbe decode (raw IMBE frames tapped from "
                "/api/imbe_dump, matched by Hamming distance). mode=A real wideband captures "
                "(whole, single pass from the TX board's SD through the fbench-agent RAM-ring "
                "relay; a_unit=window for short RAM windows), B synthetic full system (CC + "
                "concurrent traffic recordings up-converted and mixed, log-clock aligned "
                "+-30 ms), C traffic only (recordings back to back on one channel after a CC "
                "primer; follower locked, restored afterwards). Manifest from "
                "tools/p25_corpus_index.py. Resumable (resume=<run dir>|auto), stoppable "
                "(touch bench/.state/corpus/STOP, max_minutes).",
    pass_criteria="clear-voice frame recovery >= min_recovery_pct of SDRTrunk over followable "
                  "transmissions; relay underruns <= max_underruns; focus tone dropouts <= "
                  "max_tone_dropouts",
    artifacts=("corpus_run.json", "items/<item>.json", "items/<item>_audio.wav",
               "scores.json"),
    suites=(), analyze=analyze_corpus,
)(rf_p25_corpus)
