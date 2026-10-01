//! The board's health: load, memory, CPU per core, the scanner's memory and CPU per thread, and
//! the AD9361's and the Zynq's temperatures. A sampler reads `/proc` and the IIO temperatures
//! every few seconds; CPU shares are over the last interval. Off the board the readings are
//! absent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::util::time::unix_ms;

const EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default, Serialize)]
pub struct ThreadCpu {
    pub name: String,
    pub cpu_pct: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Health {
    pub at_unix_ms: u64,
    pub board_uptime_s: Option<f64>,
    /// 1, 5 and 15 minutes.
    pub load: Option<[f64; 3]>,
    pub mem_total_kb: Option<u64>,
    pub mem_available_kb: Option<u64>,
    /// Busy share of each core.
    pub core_cpu_pct: Vec<f64>,
    pub rss_kb: Option<u64>,
    pub threads: Option<u64>,
    /// The scanner's share of one core.
    pub cpu_pct: Option<f64>,
    /// The scanner's threads, busiest first.
    pub thread_cpu: Vec<ThreadCpu>,
    pub ad9361_temp_c: Option<f64>,
    pub zynq_temp_c: Option<f64>,
}

/// Busy and total jiffies of each core, from `/proc/stat`.
pub fn core_jiffies(stat: &str) -> Vec<(u64, u64)> {
    stat.lines()
        .filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
        .map(|l| {
            let v: Vec<u64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            let total: u64 = v.iter().take(8).sum();
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            (total - idle, total)
        })
        .collect()
}

/// The name and the user plus system jiffies of a `/proc/<pid>/stat` line (the name may hold
/// spaces and parentheses: the fields follow the last `)`).
pub fn task_jiffies(stat: &str) -> Option<(String, u64)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    // After the name: state is field 3, utime 14 and stime 15.
    let utime: u64 = rest.get(11)?.parse().ok()?;
    let stime: u64 = rest.get(12)?.parse().ok()?;
    Some((name, utime + stime))
}

/// A `key: value kB` line of `/proc/meminfo` or `/proc/self/status`.
pub fn kb_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find(|l| l.starts_with(key) && l[key.len()..].starts_with(':')).and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
}

/// Busy share between two readings, in percent.
fn share(busy: (u64, u64), before: (u64, u64)) -> f64 {
    let total = busy.1.saturating_sub(before.1);
    if total == 0 {
        return 0.0;
    }
    (100.0 * busy.0.saturating_sub(before.0) as f64 / total as f64 * 10.0).round() / 10.0
}

#[derive(Default)]
struct Previous {
    cores: Vec<(u64, u64)>,
    process: Option<u64>,
    threads: HashMap<String, u64>,
    total_jiffies: u64,
}

#[derive(Default)]
pub struct SystemHealth {
    health: Mutex<Health>,
}

impl SystemHealth {
    pub fn health(&self) -> Health {
        self.health.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut prev = Previous::default();
            let mut tick = tokio::time::interval(EVERY);
            loop {
                tick.tick().await;
                let h = tokio::task::spawn_blocking(move || {
                    let h = read(&mut prev);
                    (h, prev)
                })
                .await;
                match h {
                    Ok((h, p)) => {
                        prev = p;
                        *me.health.lock().unwrap_or_else(|e| e.into_inner()) = h;
                    }
                    Err(e) => {
                        tracing::warn!("system health: {e}");
                        prev = Previous::default();
                    }
                }
            }
        });
    }
}

fn file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The first IIO device named `name`.
fn iio_device(name: &str) -> Option<std::path::PathBuf> {
    std::fs::read_dir("/sys/bus/iio/devices").ok()?.flatten().map(|e| e.path()).find(|p| {
        std::fs::read_to_string(p.join("name")).is_ok_and(|n| n.trim() == name)
    })
}

fn number(path: std::path::PathBuf) -> Option<f64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read(prev: &mut Previous) -> Health {
    let mut h = Health { at_unix_ms: unix_ms(), ..Health::default() };
    h.board_uptime_s = file("/proc/uptime").and_then(|u| u.split_whitespace().next()?.parse().ok());
    h.load = file("/proc/loadavg").and_then(|l| {
        let v: Vec<f64> = l.split_whitespace().take(3).filter_map(|x| x.parse().ok()).collect();
        (v.len() == 3).then(|| [v[0], v[1], v[2]])
    });
    if let Some(m) = file("/proc/meminfo") {
        h.mem_total_kb = kb_field(&m, "MemTotal");
        h.mem_available_kb = kb_field(&m, "MemAvailable");
    }
    if let Some(s) = file("/proc/self/status") {
        h.rss_kb = kb_field(&s, "VmRSS");
        h.threads = kb_field(&s, "Threads");
    }
    if let Some(stat) = file("/proc/stat") {
        let cores = core_jiffies(&stat);
        if prev.cores.len() == cores.len() {
            h.core_cpu_pct = cores.iter().zip(&prev.cores).map(|(c, p)| share(*c, *p)).collect();
        }
        let total: u64 = cores.iter().map(|c| c.1).sum::<u64>() / cores.len().max(1) as u64;
        let elapsed = total.saturating_sub(prev.total_jiffies);
        let pct = |now: u64, before: u64| (elapsed > 0).then(|| (1000.0 * now.saturating_sub(before) as f64 / elapsed as f64).round() / 10.0);
        if let Some((_, p)) = file("/proc/self/stat").and_then(|s| task_jiffies(&s)) {
            h.cpu_pct = prev.process.and_then(|b| pct(p, b));
            prev.process = Some(p);
        }
        let mut now: HashMap<String, u64> = HashMap::new();
        if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
            for t in tasks.flatten() {
                if let Some((name, j)) = file(&format!("{}/stat", t.path().display())).and_then(|s| task_jiffies(&s)) {
                    // Threads of one name (the runtime's workers) add up.
                    *now.entry(name).or_default() += j;
                }
            }
        }
        if prev.total_jiffies > 0 {
            let mut list: Vec<ThreadCpu> = now
                .iter()
                .filter_map(|(n, j)| Some(ThreadCpu { name: n.clone(), cpu_pct: pct(*j, *prev.threads.get(n)?)? }))
                .collect();
            list.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct));
            h.thread_cpu = list;
        }
        prev.threads = now;
        prev.cores = cores;
        prev.total_jiffies = total;
    }
    h.ad9361_temp_c = iio_device("ad9361-phy").and_then(|d| number(d.join("in_temp0_input"))).map(|m| m / 1000.0);
    h.zynq_temp_c = iio_device("xadc").and_then(|d| {
        let raw = number(d.join("in_temp0_raw"))?;
        let offset = number(d.join("in_temp0_offset"))?;
        let scale = number(d.join("in_temp0_scale"))?;
        Some(((raw + offset) * scale / 1000.0 * 10.0).round() / 10.0)
    });
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_readings_parse() {
        let stat = "cpu  338216 0 31149 2499909 728 0 2071 0 0 0\ncpu0 100 0 50 800 50 0 0 0 0 0\ncpu1 10 0 10 970 10 0 0 0 0 0\nintr 1";
        assert_eq!(core_jiffies(stat), vec![(150, 1000), (20, 1000)]);
        let task = "7764 (tokio-runtime w) S 1 7764 1 0 -1 4194560 2385 0 0 0 1250 340 0 0 20 0 15 0 3069 47828992 2445";
        assert_eq!(task_jiffies(task), Some(("tokio-runtime w".to_string(), 1590)));
        assert_eq!(task_jiffies("1 (a (b) c) S 0 0 0 0 0 0 0 0 0 0 7 3 0"), Some(("a (b) c".to_string(), 10)));
        let status = "Name:\tscanner\nVmRSS:\t   10652 kB\nThreads:\t15\n";
        assert_eq!((kb_field(status, "VmRSS"), kb_field(status, "Threads")), (Some(10_652), Some(15)));
        assert_eq!(share((150, 1000), (100, 900)), 50.0);
    }
}
