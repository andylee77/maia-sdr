"""Negative and multi-step scenarios on the simulated bench."""

from __future__ import annotations

from pathlib import Path

from conftest import BOOT_LOG, FakeServices, fixture, telemetry_jsonl
from fbench.runner import build_params, load_tests, run_test
from test_catalog import run_scenario


def _run(tid: str, cfg, services: FakeServices, roles: dict, **params):
    spec = load_tests()[tid]
    return run_test(spec, cfg, services, roles, build_params(spec, params))


def test_audit_fails_on_overclock_fsbl(cfg, services) -> None:
    audit = fixture("audit")
    audit["regs"]["DDR_PLL_CTRL"] = hex((0x24 << 12) | 0x8)  # FDIV 36 -> 600 MHz
    services.agent.overrides["audit"] = audit
    services.agent.overrides["reg dump"] = {"ok": True, "regs": audit["regs"]}
    res = _run("sys.audit", cfg, services, {"dut": "A"})
    assert res.verdict == "fail"
    m = res.result["metrics"]
    assert m["ddr_pll_fdiv"] == 36 and m["ddr_clk_mhz"] == 600.0
    assert m["ddr_taa_ns"] < 13.125
    assert "tAA" in res.summary
    assert any("overclock" in w for w in res.result["warnings"])


def test_audit_fails_on_low_vccoddr(cfg, services) -> None:
    audit = fixture("audit")
    audit["xadc"]["vccoddr"] = 1.25
    services.agent.overrides["audit"] = audit
    res = _run("sys.audit", cfg, services, {"dut": "A"})
    assert res.verdict == "fail" and "VCCO_DDR" in res.summary


def test_refclk_eth_detects_phase_step_on_bounce(cfg, tmp_path: Path) -> None:
    services = FakeServices(cfg)
    services.sim.phase_step_in = "bounce"
    from test_catalog import SCENARIOS

    spec = load_tests()["rf.refclk_eth"]
    params = build_params(spec, SCENARIOS["rf.refclk_eth"]["params"])
    res = run_test(spec, cfg, services, {"tx": "A", "rx": "B"}, params)
    assert res.verdict == "fail"
    assert res.result["metrics"]["phase_steps_bounce"] >= 1
    assert res.result["metrics"]["phase_steps_idle"] == 0


def test_refclk_eth_down_phase_only_when_not_eth_managed(cfg) -> None:
    services = FakeServices(cfg)
    services.sim.agent_units = {"A", "B"}
    spec = load_tests()["rf.refclk_eth"]
    from test_catalog import SCENARIOS

    params = build_params(spec, SCENARIOS["rf.refclk_eth"]["params"])
    res = run_test(spec, cfg, services, {"tx": "B", "rx": "A"}, params)
    assert res.verdict == "pass", res.summary
    assert res.result["metrics"]["phases"] == ["idle", "down", "bounce", "load"]
    cmds = services.ssh("A").commands
    assert "ip link set eth0 down" in cmds and "ip link set eth0 up" in cmds


def test_refclk_eth_rate_change_enters_maintenance(cfg) -> None:
    services = FakeServices(cfg)
    services.sim.agent_units = {"A", "B"}
    services.sim.images["B"] = "p25"
    spec = load_tests()["rf.refclk_eth"]
    params = build_params(spec, {"capture_s": 0.05, "bounce_s": 0.1, "bounce_pad_s": 0.1,
                                 "rx_rate_hz": 1e6, "nfft": 8192})
    res = run_test(spec, cfg, services, {"tx": "A", "rx": "B"}, params)
    maint = [a for u, a in services.agent.calls if u == "B" and a[0] == "maint"]
    assert maint == [["maint", "enter"], ["maint", "exit"]]
    assert res.result["maintenance_mode"] is True
    assert services.sim.num("B", "ad9361-phy", "sampling_frequency", "voltage0", False) == 2.5e6


def test_soak_detects_periodic_irq_dips(cfg, services) -> None:
    get = services.ssh("A").get

    def periodic_get(remote, local, timeout):
        if remote.endswith("telemetry.jsonl"):
            Path(local).parent.mkdir(parents=True, exist_ok=True)
            Path(local).write_text(telemetry_jsonl(600, dip_every=10), encoding="utf-8")
        else:
            get(remote, local, timeout)

    services.ssh("A").get = periodic_get  # type: ignore[method-assign]
    res = _run("sys.soak", cfg, services, {"dut": "A"}, seconds=600)
    assert res.verdict == "fail"
    per = res.result["metrics"]["periodic_kinds"]
    assert per and abs(per[0]["period_s"] - 10.0) < 0.1 and per[0]["kind"] == "irq_dip:45"


def test_boot_log_kernel_panic_fails(cfg, services) -> None:
    services.serial_lines = BOOT_LOG[:3] + ["Kernel panic - not syncing: VFS: Unable to mount"]
    res = _run("sys.boot_log", cfg, services, {"dut": "A"}, seconds=1)
    assert res.verdict == "fail" and res.result["metrics"]["kernel_panics"] == 1


def test_boot_log_silence_is_inconclusive(cfg, services) -> None:
    services.serial_lines = []
    res = _run("sys.boot_log", cfg, services, {"dut": "A"}, seconds=1)
    assert res.verdict == "inconclusive" and res.exit_code == 5


def test_memtest_error_is_decoded_to_dq_lines(cfg, services) -> None:
    services.sim.images["A"] = "hwval"
    bad = fixture("hwval_mt")
    bad.update(err_count=5, first_err={"addr": "0x24001000", "expected": "0x0",
                                       "actual": hex(1 << 17)})
    services.agent.overrides["hwval mt run"] = lambda u, a: bad if \
        a[a.index("--pattern") + 1] == "zeros" else fixture("hwval_mt")
    res = _run("hw.memtest", cfg, services, {"dut": "A"})
    assert res.verdict == "fail"
    assert res.result["metrics"]["failing_dq_lines"] == [17]
    assert res.result["metrics"]["failing_byte_lanes"] == [2]


def test_legacy_ring_loss_uses_words_not_packer_pulses(cfg, services) -> None:
    services.sim.images["A"] = "hwval"
    rep = fixture("hwval_legacy_stop")
    rep.pop("loss_words")
    rep["regs"].update(LEGACY_PACKER_OVF=50, LEGACY_WORDS_IN=1000, LEGACY_WORDS_ACCEPTED=1000)
    services.agent.overrides["hwval legacy stop"] = rep
    res = _run("hw.legacy_ring", cfg, services, {"dut": "A"}, load=[])
    assert res.verdict == "pass"
    assert res.result["metrics"]["packer_overflow_pulses_overstates_loss"] == 50
    rep["regs"].update(LEGACY_WORDS_ACCEPTED=990, LEGACY_MAX_OUTSTANDING=5)
    res = _run("hw.legacy_ring", cfg, services, {"dut": "A"}, load=[])
    assert res.verdict == "fail" and res.result["metrics"]["words_lost"] == 10
    assert any("B cap" in w for w in res.result["warnings"])


def test_ringv2_conservation_assertion(cfg, services) -> None:
    services.sim.images["A"] = "hwval"
    res = _run("hw.ringv2_protocol", cfg, services, {"dut": "A"})
    assert res.verdict == "pass", res.result["metrics"]["failed_assertions"]
    names = [x["name"] for x in res.result["metrics"]["assertions"]]
    assert names == ["word_conservation", "drain_to_idle", "baseline_lossless", "header",
                     "flush_pads", "lap_detection", "protect_mode"]
    assert res.result["metrics"]["reader_safety_margin_bursts"] == 131072 - 1 - 8
    rep = fixture("hwval_ringv2_stop")
    rep["accounting"]["drop_full"] = 1  # one word unaccounted for
    services.agent.overrides["hwval ringv2 stop"] = rep
    res = _run("hw.ringv2_protocol", cfg, services, {"dut": "A"})
    assert res.verdict == "fail"
    assert "word_conservation" in res.result["metrics"]["failed_assertions"]


def test_isolation_two_phase(cfg, tmp_path: Path) -> None:
    services = FakeServices(cfg)
    spec = load_tests()["rf.isolation"]
    cabled = run_test(spec, cfg, services, {"tx": "A", "rx": "B"},
                      build_params(spec, {"nsamples": 65536}))
    assert cabled.verdict == "inconclusive"
    assert "phase=open" in cabled.summary
    services.sim.cable_removed = True
    open_ = run_test(spec, cfg, services, {"tx": "A", "rx": "B"},
                     build_params(spec, {"nsamples": 65536, "phase": "open",
                                         "tx_atten_db": 10.0,
                                         "reference": str(cabled.run_dir)}))
    assert open_.verdict == "pass", open_.summary
    assert open_.result["metrics"]["isolation_db"] > 60


def test_p25_replay_regression_against_baseline(cfg, tmp_path: Path) -> None:
    base, services = run_scenario("rf.p25_replay", cfg, tmp_path)
    assert base.verdict == "pass"
    services.sim.decode_rate = 20.0  # a worse build: -33 %
    spec = load_tests()["rf.p25_replay"]
    from test_catalog import _clip

    params = build_params(spec, {"clip": str(_clip(tmp_path)), "seconds": 10,
                                 "baseline": str(base.run_dir)})
    worse = run_test(spec, cfg, services, {"tx": "B", "rx": "A"}, params)
    assert worse.verdict == "fail"
    assert worse.result["metrics"]["score_vs_baseline_pct"] < -30


def _site_clip(tmp_path: Path, truth_frames: int) -> tuple[Path, Path]:
    """0.4 s SDRTrunk-named capture plus a truth dir with one transmission inside it."""
    import json
    import time

    import numpy as np

    fs, t0 = 1_000_000, 1777801424
    t = np.arange(int(0.4 * fs)) / fs
    iq = 3000 * np.exp(2j * np.pi * 50e3 * t)
    inter = np.empty(2 * t.size, dtype="<i2")
    inter[0::2], inter[1::2] = np.round(iq.real), np.round(iq.imag)
    clip = tmp_path / f"{t0}_860000000_{fs}_baseband.cs16"
    inter.tofile(clip)
    truth = tmp_path / "recordings"
    truth.mkdir()
    stamp = time.strftime("%Y%m%d_%H%M%S", time.localtime(t0))
    frames = [{"time": int((t0 + 0.1 + 0.01 * i) * 1000), "hex": "00"}
              for i in range(truth_frames)]
    (truth / f"{stamp}_860000000_1_300_1014.mbe").write_text(
        json.dumps({"to": "300", "from": "1014", "frames": frames}))
    return clip, truth


def test_p25_replay_streams_from_tx_board_and_scores_against_sdrtrunk(cfg, tmp_path) -> None:
    clip, truth = _site_clip(tmp_path, truth_frames=10)
    cfg.unit("A").ref_ppm, cfg.unit("B").ref_ppm = 0.2, -0.5
    # clip recorded by B (an explicit clip does not inherit rf.p25_clip_recorder)
    for imbe_rate, verdict in ((25.0, "pass"), (5.0, "fail")):
        services = FakeServices(cfg)
        services.sim.images["B"] = "p25"
        services.sim.agent_units = {"A", "B"}
        services.sim.imbe_rate = imbe_rate  # 25/s x 0.4 s loop = the 10 truth frames
        res = _run("rf.p25_replay", cfg, services, {"tx": "A", "rx": "B"}, clip=str(clip),
                   clip_seconds=0.4, seconds=4.0, settle_s=0.1, truth_dir=str(truth),
                   clip_recorder="B")
        m = res.result["metrics"]
        assert res.verdict == verdict, (res.summary, res.result["errors"])
        assert m["source"] == "board" and m["loops"] == 10.0
        assert m["truth_imbe_per_loop"] == 10
        assert abs(m["imbe_recovery_pct"] - 4 * imbe_rate) < 3
        ssh = services.ssh("A")
        assert any(r.startswith("/root/fbench_replay/clip_") for _, r in ssh.puts)
        assert any("setsid sh -c" in c and "iio_writedev -u local:" in c for c in ssh.commands)
        assert any("pkill -x iio_writedev" in c for c in ssh.commands)
        assert "A" not in services.sim.cyclic  # stream stopped
        calls = [" ".join(a) for u, a in services.agent.calls if u == "A"]
        assert "maint enter" in " | ".join(calls) and "maint exit" in " | ".join(calls)
        # TX LO = clip centre + (ref_B - ref_A) x f = 860 MHz - 0.7 ppm = -602 Hz
        lo = services.sim.num("A", "ad9361-phy", "frequency", "altvoltage1", True)
        assert lo == 860_000_000 - 602
    clip_json = res.run_dir / "artifacts" / "clip.json"
    assert '"lo_trim_from": "ref_ppm B -0.500 - A +0.200"' in clip_json.read_text()


def test_p25_replay_warns_without_recorder_and_rejects_long_cyclic(cfg, tmp_path) -> None:
    clip, _ = _site_clip(tmp_path, truth_frames=0)
    services = FakeServices(cfg)
    res = _run("rf.p25_replay", cfg, services, {"tx": "B", "rx": "A"}, clip=str(clip),
               clip_seconds=0.4, seconds=1.0, settle_s=0.1)
    assert res.result["metrics"]["source"] == "cyclic"  # B runs factory firmware
    assert any("clip recorder unknown" in w for w in res.result["warnings"])
    assert any("traffic not scored" in w for w in res.result["warnings"])
    big = tmp_path / "1777801424_860000000_4000000_baseband.cs16"
    big.write_bytes(b"\0" * (60 << 20))
    res = _run("rf.p25_replay", cfg, FakeServices(cfg), {"tx": "B", "rx": "A"}, clip=str(big),
               clip_seconds=4.0, source="cyclic")
    assert res.verdict == "precondition" and "cyclic DMA buffer" in res.summary


def test_p25_replay_needs_clip(cfg, services) -> None:
    res = _run("rf.p25_replay", cfg, services, {"tx": "B", "rx": "A"}, seconds=1)
    assert res.verdict == "precondition" and "clip" in res.summary


def test_rf_warns_outside_ad9363_lo_range(cfg, services) -> None:
    res = _run("rf.cw_ppm", cfg, services, {"tx": "A", "rx": "B"}, freq_hz=100e6,
               rx_retune=True, nsamples=16384, span_s=0)
    assert any("AD9363" in w and "outside" in w for w in res.result["warnings"])
    # the RX LO was restored afterwards
    assert services.sim.num("B", "ad9361-phy", "frequency", "altvoltage0", True) == 858.1e6


def test_rf_short_span_is_inconclusive(cfg, services) -> None:
    res = _run("rf.cw_ppm", cfg, services, {"tx": "A", "rx": "B"}, nsamples=16384, span_s=60,
               interval_s=30)
    assert res.verdict == "inconclusive" and "drift not assessed" in res.summary
    assert abs(res.result["metrics"]["ppm_mean"] + 1.5) < 0.01


def test_cw_ppm_checks_stored_p25_corrections(cfg) -> None:
    # sim references: A 0 ppm, B +1.5 ppm, so A->B measures -1.5 ppm
    cal_file = "/mnt/jffs2/p25-ppm-cal.json"
    for lo_ppm, verdict, resid in ((1.5, "pass", 0.0), (1.2, "fail", -0.3)):
        services = FakeServices(cfg)
        services.ssh("B").files[cal_file] = (
            f'{{"lo_shift_hz": {round(-lo_ppm * 858.1)}, "lo_ppm": {lo_ppm}, '
            '"rx_lo_hz": 858100000, "method": "manual_override"}').encode()
        res = _run("rf.cw_ppm", cfg, services, {"tx": "A", "rx": "B"}, nsamples=16384,
                   span_s=300, interval_s=300)
        m = res.result["metrics"]
        assert m["cal_A_status"] == "absent" and m["cal_A_ppm"] == 0.0
        assert m["cal_B_status"] == "stored" and m["cal_predicted_ppm"] == -lo_ppm
        assert abs(m["cal_residual_ppm"] - resid) < 0.01
        assert res.verdict == verdict, res.summary
        assert "stored p25 corrections predict" in res.summary


def test_dds_used_on_factory_tx_and_pattern_restored_on_p25(cfg) -> None:
    services = FakeServices(cfg)
    _run("rf.cw_ppm", cfg, services, {"tx": "B", "rx": "A"}, nsamples=16384, span_s=0)
    dds = [c for c in services.iio("B").calls if c[1] == "cf-ad9361-dds-core-lpc"]
    assert dds, "factory TX should use the DDS"
    services2 = FakeServices(cfg)
    _run("rf.cw_ppm", cfg, services2, {"tx": "A", "rx": "B"}, nsamples=16384, span_s=0)
    writes = [a for u, a in services2.agent.calls if u == "A" and a[:2] == ["reg", "write"]]
    assert any("DAC_CHAN0_CNTRL_7" in a for a in writes)
    # attenuation was written before the pattern source was enabled (rule 3)
    calls = [" ".join(a) for u, a in services2.agent.calls if u == "A"]
    first_gain = next(i for i, c in enumerate(calls) if "hardwaregain" in c and "set" in c)
    first_src = next(i for i, c in enumerate(calls) if "DAC_CHAN0_CNTRL_7 --value 0x1" in c)
    assert first_gain < first_src


def test_tx_writes_carry_tx_ok_and_non_tx_tests_cannot(cfg) -> None:
    services = FakeServices(cfg)
    _run("rf.cw_ppm", cfg, services, {"tx": "A", "rx": "B"}, nsamples=16384, span_s=0)
    writes = [a for u, a in services.agent.calls if a[:2] == ["reg", "write"]]
    assert writes and all("--tx-ok" in a for a in writes)
    gains = [a for u, a in services.agent.calls if a[:3] == ["iio", "attr", "set"]
             and "hardwaregain" in a and "--out" in a]
    assert gains and all("--tx-ok" in a for a in gains)
    from fbench.runner import Outcome, TestSpec

    def sneaky(ctx):
        ctx.iio_set("A", "ad9361-phy", "hardwaregain", "0", "voltage0", True, tx_ok=True)
        return Outcome("x")

    spec = TestSpec(id="t.sneaky", tier=0, units="any", maintenance=False, tx=False, params={},
                    description="", pass_criteria="", func=sneaky)
    res = run_test(spec, cfg, FakeServices(cfg), {"dut": "A"}, {})
    assert res.verdict == "refused"
