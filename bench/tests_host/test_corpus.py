"""rf.p25_corpus: inventory/planning, rendering, scoring and the simulated bench runs."""

from __future__ import annotations

import json
import struct
from pathlib import Path

import numpy as np
import pytest

from conftest import FakeServices, install_corpus, make_corpus
from fbench import corpus as cp
from fbench.analysis import p25_corpus as pc
from fbench.analysis import p25_dsp
from fbench.analysis import p25_score as sc
from fbench.analysis import sdrtrunk as st
from fbench.runner import build_params, load_tests, run_test


@pytest.fixture
def corp(tmp_path: Path) -> dict:
    return make_corpus(tmp_path / "corpus")


def _run(cfg, services, corp: dict, mode: str, **params):
    spec = load_tests()["rf.p25_corpus"]
    params = {"manifest": str(corp["manifest"]), "mode": mode, "tail_s": 2.0, **params}
    return run_test(spec, cfg, services, {"tx": "B", "rx": "A"}, build_params(spec, params))


# ---------------------------------------------------------------------------
# Inventory and plans
# ---------------------------------------------------------------------------


def test_manifest_aligns_recordings_and_plans_all_modes(corp: dict) -> None:
    man = corp["man"]
    recs = {r["kind"]: r for r in man["recordings"]}
    assert all(r["align"] == "log" for r in man["recordings"])
    # traffic log bit 0 at T0 + 8.3 s; the wav starts LOG_TO_WAV_S later
    # (the synthetic log stamps lag the air by 50 ms, as SDRTrunk's do by its latency)
    assert recs["traffic"]["start_unix"] == pytest.approx(corp["T0"] + 8.3 + pc.LOG_TO_WAV_S,
                                                          abs=0.06)
    assert len(recs["traffic"]["transmissions"]) == 2 and recs["traffic"]["bits"]
    assert recs["cc"]["overlapping_traffic"] == [recs["traffic"]["id"]]
    s = man["summary"]
    assert s["transmissions"] == 2 and s["transmissions_clear"] == 1 and s["log_aligned"] == 2
    a = man["plans"]["A"]
    assert len(a["captures"][0]["transmissions"]) == 2 and a["windows"][0]["transmissions"]
    b = man["plans"]["B"]["scenes"][0]
    assert b["focus"] and b["rate_hz"] == 3.5e6 and len(b["sources"]) == 2
    assert b["image_clearance_hz"] >= pc.IMAGE_CLEAR_HZ
    assert s["modes"]["B"]["transmissions"] == 2


def test_band_rate_keeps_tx_images_off_the_channels() -> None:
    cc, ch = 860962500.0, 858462500.0
    c, r = pc.band_rate([cc, ch])
    assert r == 3.5e6  # 3 MSPS would put a channel beyond +-0.40 fs
    assert abs(c - (cc + ch) / 2) >= 50e3  # never the midpoint: images would swap channels
    assert pc.image_clearance([cc, ch], c) >= 100e3
    for f in (cc, ch):
        assert abs(f - c) + 12.5e3 <= 0.40 * r
    c3, r3 = pc.band_rate([860962500.0, 858437500.0, 857987500.0])
    assert pc.image_clearance([860962500.0, 858437500.0, 857987500.0], c3) >= 100e3 and r3 == 4e6


def test_wav_info_survives_wrapped_riff_sizes(tmp_path: Path) -> None:
    from conftest import _wav

    p = tmp_path / "1777801424_859212970_4000000_baseband.wav"
    _wav(p, np.ones(1000, dtype=complex), 4000000)
    raw = bytearray(p.read_bytes())
    raw[40:44] = struct.pack("<I", 12)  # a > 4 GiB file's data size wraps
    p.write_bytes(bytes(raw))
    info = st.wav_info(p)
    assert info.frames == 1000 and info.data_offset == 44 and info.rate == 4000000


def test_mbe_transmissions_split_on_gaps(tmp_path: Path) -> None:
    f = tmp_path / "20260503_091125_858437500_1_300_1014.mbe"
    times = [0.0, 0.02, 0.04, 2.5, 2.52]
    f.write_text(json.dumps({"to": "300", "from": "1014", "encrypted": False,
                             "frames": [{"time": int(1e6 + t * 1000), "hex": "00"}
                                        for t in times]}))
    call, txs = st.mbe_record(f)
    assert [t["frames"] for t in txs] == [3, 2] and call["transmissions"] == [
        f.stem + "#0", f.stem + "#1"]
    enc = tmp_path / "20260503_091125_858437500_2_402_3412739_encrypted.mbe"
    enc.write_text(json.dumps({"frames": [{"time": 1, "hex": "00"}]}))
    assert st.mbe_record(enc)[0]["encrypted"] is True


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("fmt", ["cs16", "cs12", "cs8"])
def test_pack_unpack_round_trip(fmt: str) -> None:
    fs = p25_dsp.FULL_SCALE[fmt]
    x = np.array([fs, -fs - 1, 5, -7]) + 1j * np.array([-3, 0, fs, -fs - 1])
    assert np.array_equal(p25_dsp.unpack(p25_dsp.pack(x, fmt), fmt), x)


def test_mixer_places_the_channel_and_parts_render_identically(tmp_path: Path) -> None:
    from conftest import _wav

    fs_in = 50000
    t = np.arange(2 * fs_in) / fs_in
    _wav(tmp_path / "ch.wav", 3000 * np.exp(2j * np.pi * 1000 * t), fs_in)
    src = cp.MixSource("ch.wav", 860.5e6, 0.25, 0.0, 2.0, 0.01)
    mx = cp.Mixer(tmp_path, [src], 3e6, 860e6, "cs16", noise_db=200.0, seed=1)
    whole = b"".join(mx.render(0, 3_000_000))
    parts = b"".join(mx.render(0, 1_234_567)) + b"".join(mx.render(1_234_567, 1_765_433))
    assert whole == parts
    x = p25_dsp.unpack(whole, "cs16")
    assert np.abs(x[:int(0.24 * 3e6)]).max() < 1.0  # silent before at_s
    seg = x[int(0.5 * 3e6):int(0.5 * 3e6) + 1 << 18]
    f = np.fft.fftfreq(seg.size, 1 / 3e6)[np.argmax(np.abs(np.fft.fft(seg)))]
    assert abs(f - 501000) < 20


def test_capture_segments_split_at_one_gib(corp: dict) -> None:
    item = cp.build_item(corp["man"], "A", corp["man"]["plans"]["A"]["captures"][0]["id"])
    assert item.fmt == "cs12" and len(item.files) == 1 and item.files[0].samples == 4_000_000
    assert item.playlist[-1] == {"zeros": int(cp.TAIL_ZEROS_S * 400000)}
    raw = b"".join(item.files[0].render(0, 1000))
    wav = next(Path(corp["man"]["sources"]["captures"]).glob("*.wav"))
    info = st.wav_info(wav)
    ref = np.fromfile(wav, dtype="<i2", count=2000, offset=info.data_offset)
    assert np.array_equal(p25_dsp.unpack(raw, "cs12"), ref[0::2] + 1j * ref[1::2])


# ---------------------------------------------------------------------------
# Scoring
# ---------------------------------------------------------------------------


def test_match_frames_counts_near_misses_and_order() -> None:
    rng = np.random.default_rng(3)
    h = [rng.bytes(18).hex() for _ in range(10)]  # IMBE codewords are high entropy
    truth = [(0.02 * i, x) for i, x in enumerate(h)]
    flip = format(int(h[3], 16) ^ 0b111, "036x")  # 3 bit errors
    dec = [{"t": 1.0, "hex": x, "tg": 300} for x in h[:3] + [flip] + h[5:]]
    m = sc.match_frames(truth, dec)
    assert m["recovered"] == 9 and m["exact"] == 8 and m["truth"] == 10
    v = sc.vote_offset([{"t": 5.0 + 0.02 * i, "hex": x} for i, x in enumerate(h)], truth)
    assert v["offset_s"] == pytest.approx(5.0, abs=0.01)


def test_tone_continuity_finds_dropouts() -> None:
    chunks = []
    for k in range(60):
        f = 1506.0 if (k // 15) % 2 == 0 else 807.0
        amp = 0 if k in (30, 31) else 8000
        n = np.arange(160) + 160 * k
        chunks.append({"t": 0.02 * k, "pcm": (amp * np.sin(2 * np.pi * f * n / 8000)).astype(
            "<i2")})
    r = sc.tone_continuity(chunks, [807.0, 1506.0])
    assert r["found"] and r["dropouts"] == 2 and r["tone_frames"] == 60
    assert r["per_tone"]["1506.0"]["max_dev_hz"] < 3


def test_merge_dumps_repetitive_and_lossy_polls() -> None:
    fr = [{"talkgroup": 300, "hex": f"{i:03x}"} for i in range(400)]
    dumps = [{"t": k, "frames": fr[:min(400, 50 * k)][-128:]} for k in range(9)]
    m = sc.merge_dumps(dumps)
    assert len(m["frames"]) == 400 and m["gaps"] == 0
    lossy = dumps[:3] + [{"t": 9, "frames": fr[:400][-128:]}]
    assert sc.merge_dumps(lossy)["gaps"] == 1


# ---------------------------------------------------------------------------
# Simulated bench
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("mode", ["A", "B"])
def test_corpus_modes_score_every_frame(cfg, corp: dict, mode: str) -> None:
    cfg.unit("A").ref_ppm, cfg.unit("B").ref_ppm = -0.548, 0.114
    services = FakeServices(cfg)
    install_corpus(services, corp["man"], mode)
    res = _run(cfg, services, corp, mode)
    m = res.result["metrics"]
    assert res.verdict == "pass", (res.summary, res.result["errors"])
    assert m["transmissions_followable"] == 1 and m["recovered_frames"] == 90
    assert m["encrypted_transmissions"] == 1 and m["relay_underruns"] == 0
    ssh = services.ssh("B")
    start = next(c for c in ssh.commands if "replay stream" in c)
    assert "--playlist /tmp/fbench_relay/playlist.json" in start
    assert "iio_writedev -u local: -b 262144 cf-ad9361-dds-core-lpc voltage0 voltage1" in start
    assert any(r.startswith("/mnt/sd/bench/corpus/") for _, r in ssh.puts)
    calls = [" ".join(a) for u, a in services.agent.calls if u == "B"]
    gain = next(i for i, c in enumerate(calls) if "hardwaregain" in c and "set" in c)
    assert gain < next(i for i, c in enumerate(ssh.commands) if "replay stream" in c) + len(calls)
    assert "maint enter" in " | ".join(calls) and "maint exit" in " | ".join(calls)
    assert "B" not in services.sim.cyclic
    if mode == "B":
        assert m["tone_found"] and m["tone_dropouts"] == 0
        # reference-true content: trimmed by B's own reference only
        doc = json.loads(next((res.run_dir / "artifacts" / "items").glob("*.json")).read_text())
        assert doc["tx"]["trim_from"].startswith("reference-true")
        assert doc["tx"]["trim_hz"] == pytest.approx(-0.114e-6 * doc["spec"]["centre_hz"], abs=0.2)
    else:  # captures carry unit A's uncorrected reference
        doc = json.loads(next((res.run_dir / "artifacts" / "items").glob("*.json")).read_text())
        assert doc["tx"]["trim_from"] == "ref_ppm A -0.548 - B +0.114"


def test_items_param_accepts_a_comma_list(corp: dict) -> None:
    from types import SimpleNamespace

    from fbench.runner import coerce
    from fbench.tests.corpus_tests import _select

    ids = cp.item_ids(corp["man"], "B", "A")
    want = [ids[-1], ids[0]]  # two entries even from a one-item fixture
    for raw in (",".join(want), coerce(",".join(want), "all", "items")):
        ctx = SimpleNamespace(params={"a_unit": "A", "items": raw, "limit": 0})
        assert _select(ctx, corp["man"], "B") == want


def test_missed_transmission_and_underruns_fail(cfg, corp: dict) -> None:
    services = FakeServices(cfg)
    install_corpus(services, corp["man"], "A")
    services.sim.corpus_drop = {f"{corp['call']}#0"}
    services.sim.relay_underruns = 2
    res = _run(cfg, services, corp, "A")
    m = res.result["metrics"]
    assert res.verdict == "fail"
    assert m["missed_transmissions"] == 1 and m["clear_recovery_pct"] == 0.0
    assert m["relay_underruns"] == 2


def test_tone_dropouts_fail(cfg, corp: dict) -> None:
    services = FakeServices(cfg)
    install_corpus(services, corp["man"], "B", tone_gap_frames=(40, 41, 42))
    res = _run(cfg, services, corp, "B")
    assert res.verdict == "fail" and res.result["metrics"]["tone_dropouts"] == 3


def test_stage_only_uploads_once(cfg, corp: dict) -> None:
    services = FakeServices(cfg)
    install_corpus(services, corp["man"], "B")
    first = _run(cfg, services, corp, "B", stage_only=True)
    assert first.verdict == "pass" and "1 uploaded" in first.summary
    assert "B" not in services.sim.cyclic and not any(
        "replay stream" in c for c in services.ssh("B").commands)
    again = _run(cfg, services, corp, "B", stage_only=True)
    assert "0 uploaded" in again.summary
    staged = json.loads((Path(cfg.paths.state_dir) / "corpus" / "staged_B.json").read_text())
    assert len(next(iter(staged.values()))["sha256"]) == 64


def test_resume_skips_done_items_and_stop_file_stops(cfg, corp: dict) -> None:
    services = FakeServices(cfg)
    install_corpus(services, corp["man"], "A")
    first = _run(cfg, services, corp, "A")
    services2 = FakeServices(cfg)
    install_corpus(services2, corp["man"], "A")
    again = _run(cfg, services2, corp, "A", resume=str(first.run_dir))
    assert again.verdict == "pass" and again.result["metrics"]["recovered_frames"] == 90
    assert not any("replay stream" in c for c in services2.ssh("B").commands)
    stop = Path(cfg.paths.state_dir) / "corpus" / "STOP"
    stop.parent.mkdir(parents=True, exist_ok=True)
    stop.write_text("")
    services3 = FakeServices(cfg)
    install_corpus(services3, corp["man"], "A")
    res = _run(cfg, services3, corp, "A")
    assert res.verdict == "inconclusive" and res.result["metrics"]["stopped"] is True
    assert not stop.exists()


def test_missing_manifest_is_a_precondition(cfg, tmp_path: Path) -> None:
    spec = load_tests()["rf.p25_corpus"]
    res = run_test(spec, cfg, FakeServices(cfg), {"tx": "B", "rx": "A"},
                   build_params(spec, {"manifest": str(tmp_path / "none.json")}))
    assert res.verdict == "precondition" and "p25_corpus_index" in res.summary


def test_index_tool_writes_manifest_and_report(corp: dict, tmp_path: Path) -> None:
    import importlib.util

    from conftest import BENCH

    spec = importlib.util.spec_from_file_location("p25_corpus_index",
                                                  BENCH.parent / "tools" / "p25_corpus_index.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    src = corp["man"]["sources"]
    out, rep = tmp_path / "m.json", tmp_path / "r.md"
    assert mod.main(["--captures", src["captures"], "--recordings", src["recordings"],
                     "--event-logs", src["event_logs"], "--out", str(out), "--report", str(rep),
                     "--no-mp3", "--focus", corp["call"]]) == 0
    assert pc.load_manifest(out)["summary"]["transmissions"] == 2
    text = rep.read_text()
    assert "## Coverage per replay mode" in text
    assert "| B synthetic full system | 2 | 1 | 1 |" in text


def test_agent_contract_lists_replay() -> None:
    from fbench.agent import CONTRACT

    assert "replay stream" in CONTRACT and "underruns" in " ".join(CONTRACT["replay stream"]["reply"])


def test_mode_a_window_from_ram_and_purge(cfg, corp: dict) -> None:
    services = FakeServices(cfg)
    install_corpus(services, corp["man"], "A", a_unit="window")
    services.ssh("B").sizes["/root/fbench_corpus/A_previous_window.0.cs12"] = 480 << 20
    res = _run(cfg, services, corp, "A", a_unit="window", source="ram")
    assert res.verdict == "pass", res.summary
    ssh = services.ssh("B")
    assert any(r.startswith("/root/fbench_corpus/A_") for _, r in ssh.puts)
    start = next(c for c in ssh.commands if "replay stream" in c)
    assert "--ring-mb 32" in start
    assert any("rm -f /root/fbench_corpus/A_previous_window.0.cs12" in c for c in ssh.commands)
    assert res.result["metrics"]["recovered_frames"] == 90
    # purge: staging mode B with purge=true removes the window file from the SD root only
    ssh.sizes["/mnt/sd/bench/corpus/old_file.cs12"] = 10
    install_corpus(services, corp["man"], "B")
    st_ = _run(cfg, services, corp, "B", stage_only=True, purge=True)
    run = json.loads((st_.run_dir / "artifacts" / "corpus_run.json").read_text())
    assert run["purged"] == ["old_file.cs12"]
    assert any("rm -f /mnt/sd/bench/corpus/old_file.cs12" in c for c in ssh.commands)


# ---------------------------------------------------------------------------
# Scoring from the DUT's per-call counts
# ---------------------------------------------------------------------------


def test_score_is_the_duts_call_counts_not_the_hex_tap(cfg, corp: dict) -> None:
    services = FakeServices(cfg)
    install_corpus(services, corp["man"], "B")
    services.sim.corpus_hex_flip = 40  # the tap disagrees with SDRTrunk in 40 of 144 bits
    res = _run(cfg, services, corp, "B")
    m = res.result["metrics"]
    assert res.verdict == "pass", res.summary
    assert m["recovered_frames"] == 90 and m["clear_recovery_pct"] == 100.0
    assert m["hex_aligned_frames"] == 0 and m["tap_frames"] == 90


def test_followed_calls_without_frames_score_zero(cfg, corp: dict) -> None:
    services = FakeServices(cfg)
    install_corpus(services, corp["man"], "B")
    services.sim.corpus_imbe_zero = True
    res = _run(cfg, services, corp, "B")
    m = res.result["metrics"]
    assert res.verdict == "fail" and m["clear_recovery_pct"] == 0.0
    assert m["missed_transmissions"] == 1
    scores = json.loads((res.run_dir / "artifacts" / "scores.json").read_text())
    row = next(r for r in scores["items"][0]["rows"] if not r["encrypted"])
    assert row["dut_call_id"] is not None and row["recovered"] == 0


def test_tap_baseline_excludes_the_rings_older_frames(cfg, corp: dict) -> None:
    services = FakeServices(cfg)
    install_corpus(services, corp["man"], "A")
    first = _run(cfg, services, corp, "A")  # leaves 90 frames in the fake ring
    again = _run(cfg, services, corp, "A")
    doc = json.loads(next((again.run_dir / "artifacts" / "items").glob("*.json")).read_text())
    assert doc["tap"]["baseline_frames"] >= 90 and doc["tap"]["period_s"] == 0.5
    m = again.result["metrics"]
    assert first.result["metrics"]["tap_frames"] == 90 and m["recovered_frames"] == 90


def test_score_by_calls_shares_a_calls_imbe_across_its_transmissions() -> None:
    txs = [{"id": "c#0", "call": "c", "tg": 300, "src": 1014, "frames": 45, "t0": 10.0,
            "t1": 10.9, "encrypted": False},
           {"id": "c#1", "call": "c", "tg": 300, "src": 1014, "frames": 279, "t0": 13.2,
            "t1": 18.8, "encrypted": False},
           {"id": "d#0", "call": "d", "tg": 300, "src": 3402108, "frames": 81, "t0": 22.0,
            "t1": 23.6, "encrypted": False}]
    base = 1_790_000_000.0
    calls = [{"call_id": 1, "tg": 300, "source": 1014, "started_unix_ms": (base + 9.7) * 1000,
              "open_ms": 11000, "imbe": 330, "ldu": 37},
             {"call_id": 2, "tg": 300, "source": 3402108, "started_unix_ms": (base + 21.8) * 1000,
              "open_ms": 2400, "imbe": 60, "ldu": 7}]
    r = sc.score_by_calls(txs, calls)
    assert r["offset_s"] == pytest.approx(base - 0.3, abs=0.3)
    rows = r["rows"]
    assert (rows["c#0"]["recovered"], rows["c#1"]["recovered"], rows["d#0"]["recovered"]) ==         (45, 279, 60)
    c1 = next(c for c in r["calls"] if c["call_id"] == 1)
    assert c1["excess"] == 6 and c1["truth_transmissions"] == ["c#0", "c#1"]


# Bench 2026-09-27: A rebooted without a clock (calls stamped ~1970 + uptime),
# /api/ui/calls still listed yesterday's calls, and the old histogram vote tried
# to allocate 26.7 GiB for bins spanning 56 years.
def test_offset_vote_and_item_calls_survive_a_clock_jump() -> None:
    from fbench.tests.corpus_tests import _item_calls

    txs = [{"id": f"t{i}", "tg": 300, "src": 1014, "t0": 10.0 + 20 * i} for i in range(4)]
    uptime = 1_060_000.0
    calls = [{"call_id": i, "tg": 300, "source": 1014,
              "started_unix_ms": (uptime + 9.2 + 20 * i) * 1000} for i in range(4)]
    stale = [{"call_id": 90 + i, "tg": 300, "source": 1014,
              "started_unix_ms": (1_789_400_000.0 + 7 * i) * 1000} for i in range(3)]
    assert sc.vote_call_offset(calls + stale, txs) == pytest.approx(uptime - 0.8, abs=0.3)
    doc = {"calls": calls + stale, "dut_clock": {"dut_minus_mono_s": -1_000.0},
           "stream0_mono": uptime + 1_000.0, "spec": {"seconds": 80.0}}
    assert [c["call_id"] for c in _item_calls(doc)] == [0, 1, 2, 3]


# Bench 2026-09-27 run 090411 (mode B, 42 scenes): scorer artifacts found offline.

_BASE = 1_790_000_000.0
F1, F2, F3 = 857_987_500, 858_437_500, 858_462_500


def _tx(tid: str, tg: int, src: int, freq: int, t0: float, t1: float, frames: int) -> dict:
    return {"id": tid, "call": tid.split("#")[0], "tg": tg, "src": src, "freq_hz": freq,
            "t0": t0, "t1": t1, "frames": frames, "encrypted": False}


def _call(cid: int, tg: int, src: int, freq: int, s0: float, s1: float, imbe: int,
          nf: str | None = None) -> dict:
    return {"call_id": cid, "tg": tg, "source": src, "sources": [src], "freq_hz": freq,
            "started_unix_ms": (_BASE + s0) * 1000, "open_ms": int((s1 - s0) * 1000),
            "imbe": imbe, "ldu": imbe // 9, "not_followed": nf, "encrypted": False}


def test_score_by_calls_credits_the_call_that_carried_the_audio() -> None:
    txs = [  # 399: the grant named 1012, SDRTrunk kept the talker 3402084
           _tx("a#0", 300, 3402084, F2, 20.97, 26.89, 297),
           _tx("a#1", 300, 3402084, F2, 27.59, 34.61, 351),
           # 221053_245: two same-source calls both within 3 s (a tie before)
           _tx("b#0", 300, 3599061, F1, 44.34, 45.92, 81),
           # 144747_59: one transmission split over the grant's and the talker's call
           _tx("c#0", 300, 3400012, F1, 49.57, 52.94, 162),
           # 100134_365: same source, the call on the other channel must not count
           _tx("d#0", 300, 1014, F1, 56.26, 57.91, 81),
           _tx("e#0", 300, 1014, F3, 60.71, 61.79, 54)]
    calls = [_call(1, 300, 3402084, F2, 20.96, 27.30, 297),
             _call(2, 300, 1012, F2, 27.30, 35.55, 387),
             _call(3, 300, 3599061, F1, 41.48, 44.17, 27),
             _call(4, 300, 3599061, F1, 44.17, 47.13, 81),
             _call(5, 300, 1012, F1, 49.68, 50.67, 36),
             _call(6, 300, 3400012, F1, 50.67, 54.17, 126),
             _call(7, 300, 1014, F1, 56.44, 60.43, 81),
             _call(8, 300, 1014, F3, 60.96, 64.43, 54)]
    r = sc.score_by_calls(txs, calls, dut_minus_stream=_BASE)
    got = {k: v["recovered"] for k, v in r["rows"].items()}
    assert got == {"a#0": 297, "a#1": 351, "b#0": 81, "c#0": 162, "d#0": 81, "e#0": 54}
    assert r["rows"]["c#0"]["call_ids"] == [5, 6] and r["rows"]["b#0"]["call_id"] == 4
    c2 = next(c for c in r["calls"] if c["call_id"] == 2)
    assert c2["truth_transmissions"] == ["a#1"] and c2["excess"] == 36
    assert next(c for c in r["calls"] if c["call_id"] == 3)["truth_transmissions"] == []


def test_score_by_calls_falls_back_to_leftovers_only() -> None:
    # No call covers a#0 (the DUT missed it); the next call of the same source within
    # 3 s keeps its frames for its own transmission.
    txs = [_tx("a#0", 300, 1014, F1, 10.0, 11.5, 75), _tx("a#1", 300, 1014, F1, 12.5, 14.0, 75)]
    r = sc.score_by_calls(txs, [_call(1, 300, 1014, F1, 12.4, 15.0, 80)], dut_minus_stream=_BASE)
    assert (r["rows"]["a#0"]["recovered"], r["rows"]["a#1"]["recovered"]) == (5, 75)
    assert r["rows"]["a#0"]["match"] == "near" and r["rows"]["a#1"]["match"] == "cover"


def test_offset_vote_prefers_the_clock_prior() -> None:
    # 062337_342: three calls, each (call, same-source transmission) pair votes once;
    # the lowest wrong pair (call 753 vs the next 1013 transmission) won by 5.4 s.
    txs = [_tx("t1", 300, 3409510, F1, 12.56, 16.34, 189),
           _tx("t2", 300, 3400041, F1, 17.35, 22.05, 234),
           _tx("t3", 300, 1013, F1, 22.98, 24.03, 54)]
    calls = [_call(753, 300, 1013, F1, 17.55, 21.19, 171),
             _call(755, 300, 3400041, F1, 21.19, 22.98, 72),
             _call(756, 300, 1013, F1, 22.98, 26.59, 54)]
    assert sc.vote_call_offset(calls, txs) == pytest.approx(_BASE - 5.43, abs=0.05)
    prior = _BASE + 1.1  # DUT clock + stream start; votes land 0.5-2 s below it
    assert sc.vote_call_offset(calls, txs, prior=prior) == pytest.approx(_BASE, abs=0.05)
    r = sc.score_by_calls(txs, calls, prior=prior)
    assert r["rows"]["t2"]["recovered"] == 234 and r["rows"]["t2"]["call_ids"] == [753, 755]
    # Nothing inside the window: the plain vote.
    assert sc.vote_call_offset(calls, txs, prior=_BASE + 100.0) == pytest.approx(_BASE - 5.43,
                                                                               abs=0.05)


def test_item_calls_follow_a_dut_clock_step_mid_item() -> None:
    # 093344_13: A's clock went from uptime to real time ~110 s into the item.
    from fbench.tests.corpus_tests import _item_calls

    clk0, clk1 = -1_060_065.07, 1_789_453_785.57
    s0 = 1_062_046.45
    pre = [{"call_id": 569, "tg": 402, "started_unix_ms": (clk0 + s0 + 101.2) * 1000}]
    post = [{"call_id": 575 + i, "tg": 300, "started_unix_ms": (clk1 + s0 + t) * 1000,
             "ended_unix_ms": (clk1 + s0 + t + 1.7) * 1000} for i, t in enumerate((136.4, 138.1))]
    doc = {"calls": pre + post, "dut_clock": {"dut_minus_mono_s": clk0}, "stream0_mono": s0,
           "spec": {"seconds": 150.7}}
    assert [c["call_id"] for c in _item_calls(doc)] == [569]
    got = _item_calls({**doc, "dut_clock_end": {"dut_minus_mono_s": clk1}})
    assert [c["call_id"] for c in got] == [569, 575, 576]
    assert got[1]["started_unix_ms"] / 1000 - clk0 - s0 == pytest.approx(136.4, abs=1e-3)
    assert got[1]["ended_unix_ms"] - got[1]["started_unix_ms"] == pytest.approx(1700, abs=1)
    assert [c["call_id"] for c in _item_calls(doc, clock_end=clk1)] == [569, 575, 576]
    assert _item_calls(doc, clock_end=clk0 + 0.02) == pre  # no step


def test_blockers_first_come_and_sdrtrunk_channel_hold() -> None:
    o = _tx("o", 201, 3404086, F1, 36.67, 42.46, 288)  # followed
    busy = [(float(F1), 33.78, 43.73)]  # SDRTrunk's channel: grant .. teardown
    t_after = _tx("t_after", 300, 3400027, F2, 44.41, 46.21, 90)  # grant 43.91: torn down
    t_hang = _tx("t_hang", 300, 1013, F2, 43.90, 45.00, 50)  # grant 43.40: still held
    t_voice = _tx("t_voice", 301, 3406004, F3, 40.00, 41.00, 50)  # grant during O's voice
    t_same = _tx("t_same", 201, 3404012, F2, 43.00, 44.00, 50)  # same TG: the DUT retunes
    early = _tx("early", 300, 3409515, F3, 30.00, 38.00, 400)  # granted before O, missed
    txs = [o, t_after, t_hang, t_voice, t_same, early]
    blk = sc.blockers(txs, {"o": True}, busy)
    assert blk == {"t_hang": ["o"], "t_voice": ["o"]}
    assert sc.blockers(txs, {}, busy) == {}  # nothing followed, nothing blocked
    assert sc.blockers(txs, {"o": True}) == {"t_voice": ["o"]}  # without the channel spans


def test_followable_is_first_come_not_whichever_the_dut_got() -> None:
    # 094704_28 at 92 s: A held TG 318's lock 1.4 s past SDRTrunk's teardown and
    # rejected TG 300; TG 319, granted 2 s later while TG 300 was still on the
    # air, was followed. TG 300 is a miss (it was not before this change).
    from fbench.tests.corpus_tests import score_item

    clk, s0 = 1_000.0, _BASE - 1_000.0
    frames = {"318#0": (88.19, 90.46), "300#0": (92.58, 98.16), "319#0": (94.77, 96.88)}
    truth = {k: [[round(a + 0.02 * i, 3), "00" * 18] for i in range(int((b - a) / 0.02))]
             for k, (a, b) in frames.items()}
    txs = {"318#0": _tx("318#0", 318, 1014, F1, 0, 0, 0),
           "300#0": _tx("300#0", 300, 3409515, F2, 0, 0, 0),
           "319#0": _tx("319#0", 319, 3400033, F3, 0, 0, 0)}
    calls = [_call(589, 318, 1014, F1, 88.20, 92.90, len(truth["318#0"])),
             _call(590, 300, 3409515, F2, 92.59, 92.59, 0, nf="sticky_lock"),
             _call(591, 319, 3400033, F3, 94.72, 99.50, len(truth["319#0"]))]
    for c in calls:
        c["started_unix_ms"] -= 1200.0  # the usual vote-minus-prior lag
    doc = {"id": "i", "mode": "B", "truth": truth, "calls": calls,
           "dut_clock": {"dut_minus_mono_s": clk}, "stream0_mono": s0,
           "spec": {"seconds": 205.0}, "tap": {"frames": [], "baseline_frames": 0}}
    busy = [(float(F1), 87.72, 91.45), (float(F2), 92.06, 102.05), (float(F3), 94.18, 98.36)]
    rows = {r["id"]: r for r in score_item(doc, txs, busy=busy)["rows"]}
    assert rows["300#0"]["followable"] and rows["300#0"]["recovered"] == 0
    assert rows["300#0"]["blocked_by"] == [] and rows["300#0"]["conflicts"] == ["319#0"]
    assert rows["319#0"]["followable"] and rows["319#0"]["recovery_pct"] == 100.0
    assert not rows["300#0"]["dut_followed"] and rows["319#0"]["dut_followed"]


def test_align_bits_survives_repeated_codewords_and_bit_errors() -> None:
    rng = np.random.default_rng(5)
    silence = rng.bytes(18).hex()
    speech = [rng.bytes(18).hex() for _ in range(40)]
    seq = [silence] * 6 + speech
    truth = [(0.02 * i, h) for i, h in enumerate(seq)]

    def noisy(h: str, n: int) -> str:
        v = int(h, 16)
        for b in rng.choice(144, n, replace=False):
            v ^= 1 << int(b)
        return format(v, "036x")
    tapped = [{"hex": noisy(h, 3)} for h in seq]
    del tapped[20]  # one frame the tap did not see
    al = sc.align_bits(truth, tapped)
    assert al["aligned"] == len(seq) - 1 and al["mean_bit_diff"] == pytest.approx(3.0)


def test_mbe_frames_keep_file_order(tmp_path: Path) -> None:
    f = tmp_path / "20260503_054402_857987500_1_300_3436046.mbe"
    times = [0, 20, 40, 60, -84, 100, 120, 140]  # an LDU stamp stepping back
    f.write_text(json.dumps({"to": "300", "from": "1", "frames": [
        {"time": 1_000_000 + t, "hex": f"{k:036x}"} for k, t in enumerate(times)]}))
    call, txs = st.mbe_record(f)
    assert len(txs) == 1 and txs[0]["t0"] == pytest.approx(999.916)
    fr = st.truth_frames(tmp_path, txs[0])
    assert [h for _, h in fr] == [f"{k:036x}" for k in range(8)]
