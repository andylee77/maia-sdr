//! Minimal SNTP client for boot-time wall-clock sync.
//!
//! The Fishball Z7020 has no battery-backed RTC, so without NTP
//! every boot starts at whatever epoch Buildroot initialised the
//! kernel clock to (1970 or the build timestamp). Several features
//! rely on real wall-clock time and currently produce nonsense
//! timestamps for the first few minutes after boot:
//!
//! - `event_log` entries (Phase 7F) sort ahead of everything else
//! - Grant-follower records grant ages relative to system clock
//! - NID capture / BCH-t sweeps stamp their captures
//! - `/api/stats.wall_clock` reads as `1970-01-01 ...`
//!
//! BusyBox NTPD is NOT compiled into the current Tezuka build
//! (confirmed via `board/tezuka/common/busybox.config`
//! `# CONFIG_NTPD is not set`). Rather than gate this on a Tezuka
//! rebuild, we ship a sync-only SNTP client in p25-httpd so the
//! fix lives in a hot-deployable binary.
//!
//! Design is deliberately minimal:
//!
//! - Blocking `std::net::UdpSocket` with 5-second timeout per server
//! - Tries servers in order; first successful response wins
//! - Parses SNTPv4 48-byte response, extracts transmit timestamp
//! - Calls `libc::settimeofday` to set the kernel clock
//! - Returns Ok(epoch_secs) on success, or an error on total failure
//! - Caller logs but does NOT block boot on failure (offline use case)

use std::io;
use std::net::{ToSocketAddrs, UdpSocket};
use std::time::Duration;

/// NTP epoch (1900-01-01) minus Unix epoch (1970-01-01), in seconds.
/// Subtract this from an NTP timestamp to get Unix time.
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// SNTPv4 request packet. 48 bytes, all zero except byte 0:
///  LI=0 (no warning) | VN=4 (version) | Mode=3 (client)
///  0b00_100_011 = 0x23
fn build_request() -> [u8; 48] {
    let mut buf = [0u8; 48];
    buf[0] = 0x23;
    buf
}

/// Parse the 32-bit big-endian seconds field of the transmit
/// timestamp (bytes 40-43 of the SNTP response). The fractional part
/// (bytes 44-47) is ignored — 1-second wall-clock precision is fine
/// for event log ordering.
fn parse_transmit_secs(buf: &[u8; 48]) -> Option<u64> {
    let secs = u32::from_be_bytes([buf[40], buf[41], buf[42], buf[43]]);
    if secs == 0 {
        return None;
    }
    let unix_epoch = (secs as u64).checked_sub(NTP_UNIX_OFFSET)?;
    // Sanity: reject anything before 2020-01-01 (1577836800) or
    // after 2070-01-01 (3155760000). Guards against malformed
    // responses that happen to pass the zero check.
    if !(1_577_836_800..3_155_760_000).contains(&unix_epoch) {
        return None;
    }
    Some(unix_epoch)
}

/// Query a single NTP server. Returns the parsed Unix epoch on
/// success.
fn query_one(addr: &str, timeout: Duration) -> io::Result<u64> {
    // DNS resolve. Port 123 for NTP.
    let socket_addrs: Vec<_> = format!("{addr}:123").to_socket_addrs()?.collect();
    if socket_addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("DNS empty for {addr}"),
        ));
    }

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;

    let req = build_request();
    // First reachable address wins.
    let mut last_err = None;
    for sa in &socket_addrs {
        match sock.send_to(&req, sa) {
            Ok(_) => {
                let mut resp = [0u8; 48];
                match sock.recv_from(&mut resp) {
                    Ok((n, _)) if n == 48 => {
                        if let Some(epoch) = parse_transmit_secs(&resp) {
                            return Ok(epoch);
                        }
                        last_err = Some(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "NTP transmit timestamp out of range or zero",
                        ));
                    }
                    Ok((n, _)) => {
                        last_err = Some(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("short NTP response: {n} bytes"),
                        ));
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "all NTP attempts failed")
    }))
}

/// Set the system clock via `libc::settimeofday`. Unix only.
#[cfg(target_os = "linux")]
fn set_system_clock(epoch_secs: u64) -> io::Result<()> {
    let tv = libc::timeval {
        tv_sec: epoch_secs as libc::time_t,
        tv_usec: 0,
    };
    let rc = unsafe { libc::settimeofday(&tv, std::ptr::null()) };
    if rc != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
fn set_system_clock(_epoch_secs: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "settimeofday only available on Linux",
    ))
}

/// Try each server in order. First successful query wins: we set
/// the clock and return. On total failure, return the last error.
///
/// `servers` are hostnames or IPs — port is appended internally.
/// `per_server_timeout` bounds each attempt; total wall-clock cost
/// is at most `servers.len() * per_server_timeout`.
pub fn sync_system_clock(
    servers: &[&str],
    per_server_timeout: Duration,
) -> io::Result<u64> {
    let mut last_err = None;
    for server in servers {
        match query_one(server, per_server_timeout) {
            Ok(epoch) => {
                set_system_clock(epoch)?;
                return Ok(epoch);
            }
            Err(e) => {
                last_err = Some(io::Error::new(
                    e.kind(),
                    format!("{server}: {e}"),
                ));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "no NTP servers provided")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_header_byte() {
        let req = build_request();
        assert_eq!(req[0], 0x23);
        assert!(req[1..].iter().all(|&b| b == 0));
    }

    #[test]
    fn transmit_timestamp_parses() {
        let mut buf = [0u8; 48];
        // NTP epoch seconds for 2026-04-16 00:00:00 UTC:
        //   Unix epoch for that date is 1776211200
        //   NTP timestamp = 1776211200 + 2208988800 = 3985200000
        let ntp = 3_985_200_000u32;
        buf[40..44].copy_from_slice(&ntp.to_be_bytes());
        let unix = parse_transmit_secs(&buf).expect("should parse");
        assert_eq!(unix, 1_776_211_200);
    }

    #[test]
    fn rejects_zero_timestamp() {
        let buf = [0u8; 48];
        assert!(parse_transmit_secs(&buf).is_none());
    }

    #[test]
    fn rejects_out_of_range() {
        // NTP timestamp for 1980 (below the 2020 sanity floor).
        let mut buf = [0u8; 48];
        let ntp = 2_524_521_600u32; // ~1980
        buf[40..44].copy_from_slice(&ntp.to_be_bytes());
        assert!(parse_transmit_secs(&buf).is_none());
    }
}
