"""Analysis functions on synthetic data."""

from __future__ import annotations

from pathlib import Path

import numpy as np
import pytest

from fbench.analysis import eye, periodicity, ring, sigmf, tone
from fbench.analysis.bootlog import parse_boot_log
from fbench.analysis.memtest import (
    bandwidth_mbs,
    decode_first_error,
    hist_percentile_ns,
    idle_cycles_for_duty,
    reference,
)

# ---------------------------------------------------------------------------
# eye
# ---------------------------------------------------------------------------


def test_longest_run() -> None:
    assert eye.longest_run([0, 1, 1, 0, 1, 1, 1, 0]) == (4, 6, 3)
    assert eye.longest_run([0, 0]) == (-1, -1, 0)
    assert eye.longest_run([1, 1, 1]) == (0, 2, 3)


def test_lane_windows_and_common_centre() -> None:
    rows = [[0] * 5 + [1] * 12 + [0] * 15,
            [0] * 8 + [1] * 10 + [0] * 14,
            [0] * 3 + [1] * 20 + [0] * 9]
    s = eye.eye_summary(rows)
    assert [w["width_taps"] for w in s["lanes"]] == [12, 10, 20]
    assert s["window_taps_min"] == 10 and s["worst_lane"] == 1
    assert s["window_ns_min"] == pytest.approx(10 * 0.078125, abs=1e-4)
    # intersection 8..16 -> centre 12
    assert s["centre_tap"] == 12 and s["common_window_taps"] == 9


def test_eye_without_common_window() -> None:
    rows = [[1] * 5 + [0] * 27, [0] * 20 + [1] * 5 + [0] * 7]
    s = eye.eye_summary(rows)
    assert s["centre_tap"] is None and s["window_taps_min"] == 5


def test_point_margin_on_ad9361_grid() -> None:
    grid = np.zeros((16, 16), dtype=int)
    grid[:, 4:12] = 1
    m = eye.point_margin(grid, 8, 7)
    assert m["left"] == 4 and m["right"] == 5 and m["up"] is None and m["down"] is None
    assert m["margin"] == 4
    assert eye.point_margin(grid, 8, 0)["margin"] == 0
    s = eye.grid_summary(grid, (8, 7))
    assert s["pass_cells"] == 16 * 8 and s["chosen_margin"]["margin"] == 4


def test_heatmap_png(tmp_path: Path) -> None:
    out = eye.heatmap_png([[0, 1, 1], [1, 1, 0]], tmp_path / "e.png", "t", "x", "y", (0, 1))
    assert out is not None and out.stat().st_size > 1000


# ---------------------------------------------------------------------------
# tone
# ---------------------------------------------------------------------------


def _tone(fs: float, n: int, f: float, amp: float = 0.1, noise: float = 1e-3,
          seed: int = 1) -> np.ndarray:
    rng = np.random.default_rng(seed)
    t = np.arange(n) / fs
    return amp * np.exp(1j * (2 * np.pi * f * t + 0.7)) + noise * (
        rng.normal(size=n) + 1j * rng.normal(size=n)) / np.sqrt(2)


@pytest.mark.parametrize("offset_hz", [200_000 + 37.123, -150_000 - 0.5, 1234.5])
def test_tone_frequency_accuracy(offset_hz: float) -> None:
    fs, n = 2.5e6, 1 << 18
    est = tone.estimate_tone(_tone(fs, n, offset_hz), fs, search=(offset_hz, 20e3))
    assert est.freq_hz == pytest.approx(offset_hz, abs=5e-3)  # < 5 mHz
    assert est.power_dbfs == pytest.approx(-20.0, abs=0.05)
    # amplitude 0.1 vs complex noise std 1e-3 -> full-band SNR 40 dB
    assert est.snr_db == pytest.approx(40.0, abs=0.5)


def test_tone_ppm_estimation_with_known_offset() -> None:
    """A 1.5 ppm reference error at 858.3 MHz appears as -1287.45 Hz at baseband."""
    carrier, fs, n = 858.3e6, 2.5e6, 1 << 17
    offset = 200_000.0
    true_ppm = -1.5
    f_meas = offset + carrier * true_ppm * 1e-6
    est = tone.estimate_tone(_tone(fs, n, f_meas, noise=3e-3), fs, search=(offset, 20e3))
    ppm = tone.ppm(est.freq_hz - offset, carrier)
    assert ppm == pytest.approx(true_ppm, abs=1e-4)


def test_snr_and_spurs() -> None:
    fs, n = 1e6, 1 << 16
    t = np.arange(n) / fs
    iq = _tone(fs, n, 100e3, amp=0.5, noise=1e-3) + 5e-3 * np.exp(2j * np.pi * -210e3 * t)
    est = tone.estimate_tone(iq, fs, search=(100e3, 10e3))
    assert est.spurs, "spur not found"
    top = est.spurs[0]
    assert top["freq_hz"] == pytest.approx(-210e3, abs=fs / n * 2)
    assert top["dbc"] == pytest.approx(-40.0, abs=1.0)
    # 0.5 amplitude vs 1e-3 complex noise std -> 20log10(0.5/1e-3) = 54 dB
    assert est.snr_db == pytest.approx(54.0, abs=1.5)


def test_clip_fraction() -> None:
    iq = np.full(1024, 1.0 + 0.2j)
    iq[::2] = 0.3 + 0.1j
    est = tone.estimate_tone(iq, 1e6)
    assert est.clip_fraction == pytest.approx(0.5)


def test_phase_continuity_detects_steps() -> None:
    fs, n, f = 1e6, 1 << 18, 50e3
    iq = _tone(fs, n, f, amp=0.3, noise=2e-3)
    clean = tone.phase_continuity(iq, fs, f, 1e-3, 10.0)
    assert clean["n_steps"] == 0 and clean["max_step_deg"] < 3
    stepped = iq.copy()
    stepped[n // 2:] *= np.exp(1j * np.radians(35))
    res = tone.phase_continuity(stepped, fs, f, 1e-3, 10.0)
    assert res["n_steps"] == 1
    assert abs(res["steps"][0]["step_deg"]) > 20
    assert res["steps"][0]["t_s"] == pytest.approx(n / 2 / fs, abs=0.01)


def test_harmonic_aliases_and_dc_guard() -> None:
    c = tone.harmonic_aliases([25e6], 850.5e6, 2.5e6, 2.5e6, max_harmonic=40)
    rf = [x for x in c if x["path"] == "rf"]
    assert [(x["harmonic"], x["baseband_hz"]) for x in rf] == [(34, -500e3)]  # 850 MHz
    c2 = tone.harmonic_aliases([25e6], 858.1e6, 2.5e6, 2.5e6, dc_guard_hz=100.0)
    assert all(abs(x["baseband_hz"]) > 100 for x in c2)
    spurs = [{"freq_hz": 1000.0, "dbfs": -90.0, "dbc": -70.0}]
    hits = tone.match_spurs(spurs, [{"baseband_hz": 1003.0}], 5.0)
    assert len(hits) == 1


def test_linear_slope_and_noise_floor() -> None:
    assert tone.linear_slope([0, 600], [0.0, 0.3]) == pytest.approx(0.3 / 600)
    assert tone.linear_slope([1], [2]) == 0.0
    rng = np.random.default_rng(3)
    sigma = 1e-3
    iq = sigma * (rng.normal(size=1 << 16) + 1j * rng.normal(size=1 << 16)) / np.sqrt(2)
    res = tone.noise_and_spurs(iq, 1e6, nfft=4096)
    # per-Hz density = sigma^2 / fs -> -60 - 60 = -120 dBFS/Hz
    assert res["floor_dbfs_per_hz"] == pytest.approx(-120.0, abs=1.0)


# ---------------------------------------------------------------------------
# periodicity
# ---------------------------------------------------------------------------


def test_periodicity_finds_10s_dropout_among_noise() -> None:
    rng = np.random.default_rng(7)
    periodic = np.arange(3.0, 3600.0, 10.0) + rng.normal(0, 0.05, 360)
    keep = rng.random(360) > 0.3  # some occurrences missing
    events = np.concatenate([periodic[keep], rng.uniform(0, 3600, 40)])
    res = periodicity.detect_periodicity(events)
    assert res.periodic
    assert res.period_s == pytest.approx(10.0, rel=0.005)
    assert res.interval_match_fraction > 0.5


def test_periodicity_rejects_poisson_events() -> None:
    rng = np.random.default_rng(11)
    events = np.cumsum(rng.exponential(10.0, 300))
    assert not periodicity.detect_periodicity(events).periodic


def test_periodicity_needs_enough_events() -> None:
    res = periodicity.detect_periodicity([1.0, 11.0, 21.0])
    assert not res.periodic and res.period_s is None


# ---------------------------------------------------------------------------
# ring
# ---------------------------------------------------------------------------


def test_ring_summary_counts_and_rates() -> None:
    reply = {"seconds": 3600, "bytes_checked": 10 ** 9, "lost_bytes": 4096,
             "anomalies": [{"class": "lap", "t": 10.0 * i} for i in range(30)],
             "counts": {"lap": 30, "torn": 1}}
    s = ring.summarise(reply)
    assert s["total"] == 31 and s["counts"]["lap"] == 30 and s["counts"]["bit_error"] == 0
    assert s["rate_per_hour"] == pytest.approx(31.0)
    assert s["classes_seen"] == ["lap", "torn"]
    assert s["periodicity"]["periodic"] is True


def test_ring_summary_counts_from_anomalies_when_no_counts() -> None:
    s = ring.summarise({"anomalies": [{"class": "splice", "t": 1}, {"class": "splice", "t": 2}]})
    assert s["counts"]["splice"] == 2


def test_lap_onset() -> None:
    assert ring.expected_lap_onset_ms(16, 32.768) == pytest.approx(491.52)
    pts = [(100, 0), (400, 0), (450, 0), (500, 1024), (600, 8192)]
    assert ring.lap_onset(pts) == 500 and ring.last_clean_stall(pts) == 450
    assert ring.lap_onset([(100, 0)]) is None


# ---------------------------------------------------------------------------
# sigmf
# ---------------------------------------------------------------------------


def test_sigmf_roundtrip_ci16_and_cf32(tmp_path: Path) -> None:
    iq = np.array([1 + 2j, -3 - 4j, 32767 - 32768j])
    sigmf.write(tmp_path / "a", iq, 1e6, 858.1e6, "ci16_le", "t")
    back, meta = sigmf.read(tmp_path / "a.sigmf-meta")
    assert np.array_equal(back, iq)
    assert meta["global"]["core:sample_rate"] == 1e6
    assert meta["captures"][0]["core:frequency"] == 858.1e6
    sigmf.write(tmp_path / "b", iq / 4, 2e6, None, "cf32_le")
    back, _ = sigmf.read(tmp_path / "b")
    assert np.allclose(back, iq / 4)


def test_read_clip_formats(tmp_path: Path) -> None:
    import wave

    iq = (np.arange(8) - 4).astype(np.int16)
    raw = tmp_path / "1777801424_859212970_4000000_baseband.cs16"
    iq.tofile(raw)
    clip, fs, fc = sigmf.read_clip(raw)
    assert fs == 4e6 and fc == 859212970 and len(clip) == 4
    w = tmp_path / "x.wav"
    with wave.open(str(w), "wb") as wf:
        wf.setnchannels(2)
        wf.setsampwidth(2)
        wf.setframerate(2_000_000)
        wf.writeframes(iq.tobytes())
    clip, fs, fc = sigmf.read_clip(w, freq_hz=860e6)
    assert fs == 2e6 and fc == 860e6 and clip[0] == -4 - 3j


def test_prepare_replay_cuts_window_in_chunks(tmp_path: Path) -> None:
    import hashlib
    import wave

    fs = 1000
    n = np.arange(5 * fs)
    iq = np.empty(2 * n.size, dtype="<i2")
    iq[0::2] = n % 1000  # I encodes the sample index: the window is checkable
    iq[1::2] = 0
    iq[2 * 2500] = 2000  # the peak sits inside the window
    raw = tmp_path / f"1777801424_859212970_{fs}_baseband.cs16"
    iq.tofile(raw)
    out = tmp_path / "out.cs16"
    info = sigmf.prepare_replay(raw, out, start_s=2.0, seconds=1.5, full_scale=1000.0, chunk=256)
    y = np.fromfile(out, dtype="<i2")
    assert info["samples"] == 1500 and y.size == 3000
    assert info["rate_hz"] == fs and info["freq_hz"] == 859212970
    assert info["peak_counts"] == 2000.0
    assert y[0] == 0 and y[4] == 1  # window starts at sample 2000 (I=0); 2002 -> 2 x 0.5
    assert y[2 * 500] == 1000  # the 2000-count peak lands on full scale
    assert info["sha256"] == hashlib.sha256(out.read_bytes()).hexdigest()
    w = tmp_path / "clip.wav"
    with wave.open(str(w), "wb") as wf:
        wf.setnchannels(2)
        wf.setsampwidth(2)
        wf.setframerate(fs)
        wf.writeframes(iq.tobytes())
    info_w = sigmf.prepare_replay(w, tmp_path / "w.cs16", start_s=2.0, seconds=1.5,
                                  freq_hz=860e6, full_scale=1000.0, chunk=333)
    assert info_w["sha256"] == info["sha256"] and info_w["freq_hz"] == 860e6


def test_mbe_truth_counts_frames_in_window(tmp_path: Path) -> None:
    import json
    import time

    from fbench.analysis.p25_truth import clip_epoch, mbe_truth

    t0 = clip_epoch("1777801424_859212970_4000000_baseband.wav")
    assert t0 == 1777801424.0

    def mbe(start: float, n: int, name_freq: int, frm: str, enc: bool = False) -> None:
        stamp = time.strftime("%Y%m%d_%H%M%S", time.localtime(start))
        frames = [{"time": int((start + 0.02 * i) * 1000), "hex": "00"} for i in range(n)]
        (tmp_path / f"{stamp}_{name_freq}_1_300_{frm}.mbe").write_text(
            json.dumps({"to": "300", "from": frm, "encrypted": enc, "frames": frames}))

    mbe(t0 + 4.0, 81, 857987500, "1")        # inside
    mbe(t0 + 9.5, 50, 858437500, "2")        # straddles the end (t0 + 10)
    mbe(t0 + 30.0, 72, 858437500, "3")       # after the window
    mbe(t0 + 5.0, 10, 857987500, "4", True)  # inside, encrypted
    r = mbe_truth(tmp_path, t0, t0 + 10.0)
    assert r["transmissions"] == 3
    assert r["imbe"] == 81 + 25 + 10 and r["imbe_clear"] == 81 + 25
    straddle = [c for c in r["calls"] if c["from"] == "2"][0]
    assert straddle["straddles"] and straddle["frames"] == 25 and straddle["frames_total"] == 50
    assert not [c for c in r["calls"] if c["from"] == "1"][0]["straddles"]


# ---------------------------------------------------------------------------
# boot log / memtest decoding
# ---------------------------------------------------------------------------


def test_boot_log_parsing() -> None:
    lines = ["2026-09-26T10:00:00.000-04:00\tU-Boot 2016.07 (Apr 30 2026)",
             "2026-09-26T10:00:02.000-04:00\tLinux version 6.1.0-tezuka (x) #1",
             "2026-09-26T10:00:03.000-04:00\tusb 1-1: g_ether: failed to start RNDIS",
             "2026-09-26T10:00:04.000-04:00\tUnable to mount /mnt/sd",
             "2026-09-26T10:00:09.000-04:00\tKernel panic - not syncing: Attempted to kill init"]
    info = parse_boot_log(lines)
    assert info["uboot_version"] == "2016.07"
    assert info["kernel_version"] == "6.1.0-tezuka"
    assert info["counts"]["kernel_panic"] == 1 and info["counts"]["unable_to"] == 1
    assert info["counts"]["usb_gadget"] == 1 and info["counts"]["failed"] == 1
    assert info["firmware_family"] == "tezuka" and not info["reached_login"]
    assert info["examples"]["kernel_panic"][0]["ts"].startswith("2026-09-26T10:00:09")


def test_memtest_helpers() -> None:
    assert idle_cycles_for_duty(16, 0) is None
    assert idle_cycles_for_duty(16, 100) == 0
    assert idle_cycles_for_duty(16, 50) == 16
    assert idle_cycles_for_duty(16, 25) == 48
    assert bandwidth_mbs(987_000_000, 125_000_000) == pytest.approx(987.0)
    hist = [0] * 16
    hist[5] = 99
    hist[10] = 1
    assert hist_percentile_ns(hist, 0.99) == 2 ** 6 * 8.0
    assert hist_percentile_ns(hist, 1.0) == 2 ** 11 * 8.0


def test_decode_first_error_lanes() -> None:
    run = {"err_count": 1, "first_err_addr": "0x24000040", "first_err_exp": "0x0",
           "first_err_act": hex((1 << 3) | (1 << 40)), "mode": 2, "pattern": 5, "seed": 0}
    d = decode_first_error(run)
    assert d["bits"] == [3, 40]
    assert d["dq_lines"] == [3, 8] and d["byte_lanes"] == [0, 1]
    if reference() is not None:  # .venv-hdl has amaranth; system Python may not
        assert d["reference_agrees"] is True
    assert decode_first_error({"err_count": 0}) is None
