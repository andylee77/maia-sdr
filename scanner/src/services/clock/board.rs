//! The board clock: internet time (SNTP), and stepping or slewing the kernel clock.

use std::io;
use std::net::{ToSocketAddrs, UdpSocket};
use std::time::Duration;

/// The NTP epoch (1900) before the Unix epoch, in seconds.
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// Unix seconds from an SNTP reply's transmit timestamp; `None` for a zero or implausible one
/// (before 2020 or after 2070).
fn transmit_secs(reply: &[u8; 48]) -> Option<u64> {
    let secs = u64::from(u32::from_be_bytes([reply[40], reply[41], reply[42], reply[43]]));
    let unix = secs.checked_sub(NTP_UNIX_OFFSET)?;
    (secs != 0 && (1_577_836_800..3_155_760_000).contains(&unix)).then_some(unix)
}

/// The time from one server (unix seconds).
fn query(server: &str, timeout: Duration) -> io::Result<u64> {
    let addrs: Vec<_> = format!("{server}:123").to_socket_addrs()?.collect();
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    // Version 4, client.
    let mut req = [0u8; 48];
    req[0] = 0x23;
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{server}: no address"));
    for a in &addrs {
        if let Err(e) = sock.send_to(&req, a) {
            last = e;
            continue;
        }
        let mut reply = [0u8; 48];
        match sock.recv_from(&mut reply) {
            Ok((48, _)) => match transmit_secs(&reply) {
                Some(s) => return Ok(s),
                None => last = io::Error::new(io::ErrorKind::InvalidData, "an implausible transmit time"),
            },
            Ok((n, _)) => last = io::Error::new(io::ErrorKind::InvalidData, format!("a {n}-byte reply")),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Set the clock from the first server that answers; the time set (unix seconds).
pub fn sync_ntp(servers: &[&str], timeout: Duration) -> io::Result<u64> {
    let mut last = io::Error::new(io::ErrorKind::InvalidInput, "no servers");
    for s in servers {
        match query(s, timeout) {
            Ok(secs) => {
                step(secs * 1_000)?;
                return Ok(secs);
            }
            Err(e) => last = io::Error::new(e.kind(), format!("{s}: {e}")),
        }
    }
    Err(last)
}

/// Set the clock to `unix_ms` (a jump).
#[cfg(target_os = "linux")]
pub fn step(unix_ms: u64) -> io::Result<()> {
    let tv = libc::timeval { tv_sec: (unix_ms / 1_000) as libc::time_t, tv_usec: ((unix_ms % 1_000) * 1_000) as libc::suseconds_t };
    // SAFETY: `tv` is a valid timeval; a null timezone is allowed.
    if unsafe { libc::settimeofday(&tv, std::ptr::null()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Slew the clock by `delta_ms`: the kernel runs it 500 ppm fast or slow until the offset is used
/// up, so it never jumps. Replaces a slew still pending.
#[cfg(target_os = "linux")]
pub fn slew(delta_ms: i64) -> io::Result<()> {
    let tv = libc::timeval { tv_sec: (delta_ms / 1_000) as libc::time_t, tv_usec: ((delta_ms % 1_000) * 1_000) as libc::suseconds_t };
    // SAFETY: `tv` is a valid timeval; the old delta is not wanted.
    if unsafe { libc::adjtime(&tv, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn step(_unix_ms: u64) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "the clock is set on the board only"))
}

#[cfg(not(target_os = "linux"))]
pub fn slew(_delta_ms: i64) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "the clock is set on the board only"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transmit_times_are_checked() {
        let mut r = [0u8; 48];
        assert_eq!(transmit_secs(&r), None);
        // 2026-05-03 08:42:00 UTC.
        let ntp = (1_777_797_720 + NTP_UNIX_OFFSET) as u32;
        r[40..44].copy_from_slice(&ntp.to_be_bytes());
        assert_eq!(transmit_secs(&r), Some(1_777_797_720));
        r[40..44].copy_from_slice(&((1_000_000_000 + NTP_UNIX_OFFSET) as u32).to_be_bytes());
        assert_eq!(transmit_secs(&r), None, "2001: implausible");
    }
}
