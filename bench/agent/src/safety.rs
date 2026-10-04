//! Safety machinery (design doc section 3):
//!
//! * signal handling: SIGINT/SIGTERM/SIGHUP set a stop flag that every long
//!   loop polls, so Drop guards run on the way out; SIGBUS/SIGSEGV (a bad
//!   bus access) print a JSON error, restore max TX attenuation with
//!   async-signal-safe syscalls if TX was armed, and `_exit(2)`;
//! * `tx_off()`: max attenuation, DAC sources to zero, DDS scale 0,
//!   loopback / BIST off;
//! * `TxGuard`: writes the attenuation first, runs `tx_off()` on drop;
//! * `Cleanup`: ordered restore actions run on drop (or explicitly);
//! * maintenance mode: stop/start the image's radio daemon (`S60scanner`), state in
//!   /tmp/fbench_maint.json.

use crate::access::{RegAccess, WriteOpts};
use crate::err::{AResult, AgentError, Code};
use crate::iio::{self, TX_ATTEN_MAX_DB};
use crate::regio::PhysMap;
use crate::regmap::MapSet;
use crate::sys;
use crate::util;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static STOP: AtomicBool = AtomicBool::new(false);
static TX_ARMED: AtomicBool = AtomicBool::new(false);

pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

#[cfg(test)]
pub fn reset_stop_for_tests() {
    STOP.store(false, Ordering::SeqCst);
}

pub fn check_stop() -> AResult<()> {
    if stop_requested() {
        Err(AgentError::new(Code::Interrupted, "interrupted by signal"))
    } else {
        Ok(())
    }
}

/// Sleeps in short slices, returning early (with an error) on a signal.
pub fn sleep_ms(ms: u64) -> AResult<()> {
    let end = Instant::now() + Duration::from_millis(ms);
    loop {
        check_stop()?;
        let now = Instant::now();
        if now >= end {
            return Ok(());
        }
        std::thread::sleep((end - now).min(Duration::from_millis(20)));
    }
}

pub fn sleep_us(us: u64) {
    std::thread::sleep(Duration::from_micros(us));
}

// ── signals ───────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod sig {
    use super::*;

    const EMERG_LEN: usize = 256;
    static mut EMERG_PATH: [u8; EMERG_LEN] = [0; EMERG_LEN];

    /// Stores the TX attenuation attribute path (NUL-terminated) for the
    /// fault handler.
    pub fn set_emergency_path(p: &str) {
        let b = p.as_bytes();
        if b.len() + 1 >= EMERG_LEN {
            return;
        }
        // SAFETY: written before TX is armed, only read by the handler.
        unsafe {
            let dst = std::ptr::addr_of_mut!(EMERG_PATH) as *mut u8;
            std::ptr::copy_nonoverlapping(b.as_ptr(), dst, b.len());
            *dst.add(b.len()) = 0;
        }
    }

    extern "C" fn on_term(_s: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }

    extern "C" fn on_fault(s: libc::c_int) {
        unsafe {
            if TX_ARMED.load(Ordering::SeqCst) {
                let p = std::ptr::addr_of!(EMERG_PATH) as *const libc::c_char;
                if *p != 0 {
                    let fd = libc::open(p, libc::O_WRONLY);
                    if fd >= 0 {
                        let v = b"-89.75";
                        libc::write(fd, v.as_ptr() as *const libc::c_void, v.len());
                        libc::close(fd);
                    }
                }
            }
            let msg: &[u8] = if s == libc::SIGBUS {
                b"{\"ok\":false,\"code\":\"error\",\"error\":\"SIGBUS: bus error during a hardware access (unmapped or hung AXI slave?)\"}\n"
            } else {
                b"{\"ok\":false,\"code\":\"error\",\"error\":\"SIGSEGV: invalid memory access\"}\n"
            };
            libc::write(1, msg.as_ptr() as *const libc::c_void, msg.len());
            libc::_exit(2);
        }
    }

    pub fn install() {
        unsafe {
            for (s, h) in [
                (libc::SIGINT, on_term as *const () as usize),
                (libc::SIGTERM, on_term as *const () as usize),
                (libc::SIGHUP, on_term as *const () as usize),
                (libc::SIGBUS, on_fault as *const () as usize),
                (libc::SIGSEGV, on_fault as *const () as usize),
            ] {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = h;
                sa.sa_flags = 0;
                libc::sigemptyset(&mut sa.sa_mask);
                libc::sigaction(s, &sa, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod sig {
    pub fn set_emergency_path(_p: &str) {}
    pub fn install() {}
}

pub fn install_handlers() {
    sig::install();
}

// ── TX off ────────────────────────────────────────────────────────────

fn action(name: &str, r: AResult<Value>) -> Value {
    match r {
        Ok(v) => json!({"action": name, "ok": true, "detail": v}),
        Err(e) => json!({"action": name, "ok": false, "code": e.code.as_str(), "error": e.msg}),
    }
}

/// Writes 0 to every DDS scale attribute of the DDS core (best effort).
fn dds_scale_zero() -> AResult<Value> {
    let dev = iio::find(iio::DAC)?;
    let mut done = Vec::new();
    for e in std::fs::read_dir(&dev.path)?.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with("out_altvoltage") && n.ends_with("_scale") && !n.contains("available") {
            if std::fs::write(e.path(), b"0").is_ok() {
                done.push(n);
            }
        }
    }
    Ok(json!(done))
}

/// DAC DATA_SEL = 3 (zero) on channels 0-1 through the allow-listed map.
pub fn dac_zero(maps: &MapSet) -> AResult<Value> {
    if !sys::iio_present(iio::DAC) {
        return Ok(json!("no DAC core (cf-ad9361-dds-core-lpc absent)"));
    }
    let core = maps.core("adi_dac")?;
    let mut pm = PhysMap::open(core.base, core.size as usize, true)?;
    let mut a = RegAccess::new(core, &mut pm);
    let opts = WriteOpts {
        tx_ok: true,
        ..Default::default()
    };
    for ch in 0..2 {
        a.write_opts(&format!("DAC_CHAN{ch}_CNTRL_7"), 3, opts)?;
    }
    Ok(json!({"data_sel": [a.read("DAC_CHAN0_CNTRL_7")? & 0xF, a.read("DAC_CHAN1_CNTRL_7")? & 0xF]}))
}

/// Resets the AD9361 BIST/loopback state (RX-stream corrupting modes).
pub fn bist_off() -> Vec<Value> {
    let mut out = Vec::new();
    let phy = match iio::Ad9361::open() {
        Ok(p) => p,
        Err(e) => {
            out.push(action("ad9361", Err(e)));
            return out;
        }
    };
    out.push(action("loopback=0", phy.loopback(0).map(|_| json!(0))));
    out.push(action("bist_prbs=0", phy.bist_prbs(0).map(|_| json!(0))));
    out.push(action("bist_tone=off", phy.bist_tone("0 0 0 0").map(|_| json!("0 0 0 0"))));
    out
}

/// Emergency TX shutdown. `ok` is true when the attenuation is at maximum
/// (or there is no TX device) and the DAC outputs zero (or there is no DAC).
pub fn tx_off(maps: &MapSet, lo_powerdown: bool) -> Value {
    let mut actions = Vec::new();
    let mut atten_rb = None;
    let atten_ok = match iio::Ad9361::open() {
        Ok(phy) => {
            let r = phy.set_tx_atten_db(TX_ATTEN_MAX_DB).map(|_| json!(TX_ATTEN_MAX_DB));
            let mut ok = r.is_ok();
            actions.push(action("tx_atten=-89.75dB", r));
            if phy.dev.path.join("out_voltage1_hardwaregain").exists() {
                actions.push(action(
                    "tx2_atten=-89.75dB",
                    phy.set_attr("out_voltage1_hardwaregain", "-89.75").map(|_| json!(TX_ATTEN_MAX_DB)),
                ));
            }
            atten_rb = phy.tx_atten_db();
            if let Some(v) = atten_rb {
                ok = ok && v <= TX_ATTEN_MAX_DB + 0.01;
            }
            ok
        }
        Err(e) => {
            let ok = e.code == Code::NoDevice;
            actions.push(action("tx_atten", Err(e)));
            ok
        }
    };
    let dz = dac_zero(maps);
    let dac_ok = dz.is_ok();
    actions.push(action("dac_data_sel=zero", dz));
    actions.push(action("dds_scale=0", dds_scale_zero()));
    actions.extend(bist_off());
    if lo_powerdown {
        let r = iio::Ad9361::open().and_then(|p| {
            p.set_attr("out_altvoltage1_TX_LO_powerdown", "1").map(|_| json!(1))
        });
        actions.push(action("tx_lo_powerdown=1", r));
    }
    json!({
        "ok": atten_ok && dac_ok,
        "tx_atten_db": atten_rb,
        "actions": actions,
    })
}

/// Arms TX: maximum attenuation first, then the caller may enable a
/// source. Dropping the guard runs `tx_off`.
pub struct TxGuard<'m> {
    maps: &'m MapSet,
}

impl<'m> TxGuard<'m> {
    pub fn arm(maps: &'m MapSet) -> AResult<TxGuard<'m>> {
        let phy = iio::Ad9361::open()?;
        let p = phy.dev.path.join("out_voltage0_hardwaregain");
        sig::set_emergency_path(&p.to_string_lossy());
        phy.set_tx_atten_db(TX_ATTEN_MAX_DB)?;
        TX_ARMED.store(true, Ordering::SeqCst);
        Ok(TxGuard { maps })
    }
}

impl Drop for TxGuard<'_> {
    fn drop(&mut self) {
        let r = tx_off(self.maps, false);
        TX_ARMED.store(false, Ordering::SeqCst);
        if r["ok"] != json!(true) {
            eprintln!("fbench-agent: WARNING tx_off on exit incomplete: {r}");
        }
    }
}

// ── generic cleanup guard ─────────────────────────────────────────────

type Action<'a> = Box<dyn FnOnce() -> AResult<Value> + 'a>;

/// Ordered restore actions; run in reverse on drop (even on panic/signal).
#[derive(Default)]
pub struct Cleanup<'a> {
    actions: Vec<(String, Action<'a>)>,
}

impl<'a> Cleanup<'a> {
    pub fn new() -> Self {
        Cleanup { actions: Vec::new() }
    }

    pub fn push(&mut self, name: impl Into<String>, f: impl FnOnce() -> AResult<Value> + 'a) {
        self.actions.push((name.into(), Box::new(f)));
    }

    pub fn run(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        while let Some((name, f)) = self.actions.pop() {
            out.push(action(&name, f()));
        }
        out
    }
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let res = self.run();
        for r in res {
            if r["ok"] != json!(true) {
                eprintln!("fbench-agent: cleanup failed: {r}");
            }
        }
    }
}

// ── maintenance mode ──────────────────────────────────────────────────

pub const MAINT_FILE: &str = "/tmp/fbench_maint.json";

/// The radio daemon a Fishball scanner image runs, with its init script; maintenance mode
/// stops it. The Maia and hwval images have none.
const DAEMONS: &[(&str, &str)] = &[("scanner", "/etc/init.d/S60scanner")];

/// The image's radio daemon: the first whose init script is installed.
fn installed_daemon() -> Option<(&'static str, &'static str)> {
    DAEMONS.iter().copied().find(|(_, init)| std::path::Path::new(init).exists())
}

/// Pids of the radio daemon, whichever runs.
pub fn daemon_pids() -> Vec<u32> {
    DAEMONS.iter().flat_map(|(name, _)| sys::find_procs(name)).map(|p| p.pid).collect()
}

/// The radio daemon's name for messages: the running one, else the installed one.
pub fn daemon_name() -> &'static str {
    DAEMONS
        .iter()
        .find(|(name, _)| !sys::find_procs(name).is_empty())
        .map(|(name, _)| *name)
        .or(installed_daemon().map(|(name, _)| name))
        .unwrap_or("the radio daemon")
}

pub fn maint_state() -> Option<Value> {
    let t = std::fs::read_to_string(MAINT_FILE).ok()?;
    let v: Value = serde_json::from_str(&t).ok()?;
    // A state file from a previous boot is stale.
    if v.get("boot_id").and_then(|b| b.as_str()) != sys::boot_id().as_deref() {
        return None;
    }
    Some(v)
}

pub fn in_maintenance() -> bool {
    maint_state().is_some()
}

fn run_init(init: &str, arg: &str) -> AResult<Value> {
    if !std::path::Path::new(init).exists() {
        return Err(AgentError::new(Code::Precondition, format!("{init} not found")));
    }
    let out = std::process::Command::new("/bin/sh")
        .arg(init)
        .arg(arg)
        .output()
        .map_err(|e| AgentError::new(Code::Error, format!("{init} {arg}: {e}")))?;
    Ok(json!({
        "cmd": format!("{init} {arg}"),
        "rc": out.status.code(),
        "stdout": String::from_utf8_lossy(&out.stdout).trim(),
        "stderr": String::from_utf8_lossy(&out.stderr).trim(),
    }))
}

fn wait_procs(want_running: bool, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    loop {
        let running = !daemon_pids().is_empty();
        if running == want_running {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn maint_status_json() -> Value {
    let st = maint_state();
    json!({
        "maintenance": st.is_some(),
        "state": st,
        "services": sys::services_json(),
        "daemon": installed_daemon().map(|(name, _)| name),
        "daemon_pids": daemon_pids(),
    })
}

pub fn maint_enter() -> AResult<Value> {
    if let Some(st) = maint_state() {
        let mut v = maint_status_json();
        v["already"] = json!(true);
        v["state"] = st;
        return Ok(v);
    }
    let pids = daemon_pids();
    let was_running = !pids.is_empty();
    let daemon = installed_daemon();
    if was_running && daemon.is_none() {
        return Err(AgentError::new(
            Code::Precondition,
            format!("{} runs but no init script for it is installed", daemon_name()),
        ));
    }
    let mut steps = Vec::new();
    // Record the state first so an interrupted enter can still be exited.
    let state = json!({
        "entered": util::iso_now(),
        "boot_id": sys::boot_id(),
        "was_running": was_running,
        "daemon": daemon.map(|(name, _)| name),
        "init": daemon.map(|(_, init)| init),
        "pids": pids,
        "agent_pid": std::process::id(),
    });
    util::write_file(MAINT_FILE, serde_json::to_string_pretty(&state)?.as_bytes())?;
    if let (true, Some((name, init))) = (was_running, daemon) {
        steps.push(run_init(init, "stop")?);
        if !wait_procs(false, Duration::from_secs(5)) {
            // Escalate: SIGTERM, then SIGKILL the leftovers.
            for pid in daemon_pids() {
                kill(pid, false);
            }
            if !wait_procs(false, Duration::from_secs(2)) {
                for pid in daemon_pids() {
                    kill(pid, true);
                }
                if !wait_procs(false, Duration::from_secs(2)) {
                    return Err(AgentError::new(Code::Error, format!("{name} did not stop")));
                }
            }
            steps.push(json!(format!("{name} needed signals to stop")));
        }
    }
    let mut v = maint_status_json();
    v["steps"] = json!(steps);
    v["stopped"] = json!(was_running);
    Ok(v)
}

#[cfg(target_os = "linux")]
fn kill(pid: u32, hard: bool) {
    unsafe {
        libc::kill(pid as i32, if hard { libc::SIGKILL } else { libc::SIGTERM });
    }
}

#[cfg(not(target_os = "linux"))]
fn kill(_pid: u32, _hard: bool) {}

pub fn maint_exit() -> AResult<Value> {
    let st = maint_state();
    let mut steps = bist_off();
    let was_running = st
        .as_ref()
        .and_then(|s| s.get("was_running"))
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    // Restart the daemon that was stopped: the one the state names, else the installed one.
    let recorded = st.as_ref().and_then(|s| {
        let name = s.get("daemon")?.as_str()?;
        DAEMONS.iter().copied().find(|(n, _)| *n == name)
    });
    let mut started = false;
    if let (true, Some((name, init))) = (was_running && daemon_pids().is_empty(), recorded.or(installed_daemon())) {
        steps.push(run_init(init, "start")?);
        started = wait_procs(true, Duration::from_secs(5));
        if !started {
            steps.push(json!(format!("{name} did not appear within 5 s (check /var/log/{name}.log)")));
        }
    }
    let _ = std::fs::remove_file(MAINT_FILE);
    let mut v = maint_status_json();
    v["was_in_maintenance"] = json!(st.is_some());
    v["restarted"] = json!(started);
    v["steps"] = json!(steps);
    Ok(v)
}

/// Guard that leaves maintenance mode on drop (for --auto-maint).
pub struct MaintGuard;

impl Drop for MaintGuard {
    fn drop(&mut self) {
        if let Err(e) = maint_exit() {
            eprintln!("fbench-agent: maintenance exit failed: {}", e.msg);
        }
    }
}

/// Enforces rule 4 for operations that corrupt the RX stream.
pub fn require_maintenance(auto: bool, ignore: bool, warnings: &mut Vec<String>) -> AResult<Option<MaintGuard>> {
    let running = !daemon_pids().is_empty();
    if !running {
        return Ok(None);
    }
    if auto {
        maint_enter()?;
        return Ok(Some(MaintGuard));
    }
    let name = daemon_name();
    if ignore {
        warnings.push(format!("{name} is running (maintenance ignored by --ignore-maint)"));
        return Ok(None);
    }
    Err(AgentError::new(
        Code::Precondition,
        format!("{name} is running: this operation replaces the RX stream; run `maint enter` first or pass --auto-maint"),
    ))
}
