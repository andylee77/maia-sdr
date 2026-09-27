//! `net serve|send`: TCP throughput between agents or to the host.
//!
//! `net serve --port P [--mb N] [--source] [--timeout-s T]` accepts one
//! connection and sinks data until EOF (or sources N MB with --source).
//! `net send --host H --port P --mb N [--sink]` connects and sources N MB
//! (or sinks with --sink).

use super::{sub, Ctx};
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use crate::safety;
use crate::util::{round1, round3};
use serde_json::{json, Value};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

const BUF: usize = 256 * 1024;

fn sink(s: &mut TcpStream, limit: Option<u64>) -> AResult<u64> {
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut buf = vec![0u8; BUF];
    let mut n = 0u64;
    loop {
        safety::check_stop()?;
        if let Some(l) = limit {
            if n >= l {
                break;
            }
        }
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => n += k as u64,
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => continue,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
}

fn source(s: &mut TcpStream, bytes: u64) -> AResult<u64> {
    let buf: Vec<u8> = (0..BUF).map(|i| i as u8).collect();
    let mut n = 0u64;
    while n < bytes {
        safety::check_stop()?;
        let k = ((bytes - n) as usize).min(BUF);
        s.write_all(&buf[..k])?;
        n += k as u64;
    }
    s.flush()?;
    let _ = s.shutdown(std::net::Shutdown::Write);
    Ok(n)
}

fn result(dir: &str, peer: String, bytes: u64, secs: f64) -> Value {
    json!({
        "direction": dir,
        "peer": peer,
        "bytes": bytes,
        "seconds": round3(secs),
        "mbs": round1(bytes as f64 / 1e6 / secs.max(1e-9)),
        "mbits": round1(bytes as f64 * 8.0 / 1e6 / secs.max(1e-9)),
    })
}

pub fn run(_ctx: &Ctx, args: &Args) -> AResult<Value> {
    let op = sub(args, 1, &["serve", "send"])?;
    let port = args.u64_or("port", 5201)? as u16;
    let mb = args.u64_or("mb", 100)?;
    let timeout = args.f64_or("timeout-s", 60.0)?;
    let bind = args.opt_or("bind", "0.0.0.0")?;
    let host = if op == "send" { Some(args.req("host")?) } else { None };
    let reverse = if op == "serve" { args.flag("source") } else { args.flag("sink") };
    args.finish()?;
    let bytes = mb << 20;
    if op == "serve" {
        let l = TcpListener::bind((bind.as_str(), port))
            .map_err(|e| AgentError::new(Code::Precondition, format!("bind {bind}:{port}: {e}")))?;
        l.set_nonblocking(true)?;
        let end = Instant::now() + Duration::from_secs_f64(timeout);
        let (mut s, peer) = loop {
            safety::check_stop()?;
            match l.accept() {
                Ok(x) => break x,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() > end {
                        return Err(AgentError::new(Code::Precondition, format!("no connection on port {port} within {timeout} s")));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e.into()),
            }
        };
        s.set_nonblocking(false)?;
        let _ = s.set_nodelay(true);
        let t0 = Instant::now();
        let n = if reverse { source(&mut s, bytes)? } else { sink(&mut s, None)? };
        Ok(result(if reverse { "source" } else { "sink" }, peer.to_string(), n, t0.elapsed().as_secs_f64()))
    } else {
        let host = host.unwrap();
        let addr = (host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| AgentError::new(Code::Usage, format!("resolve {host}: {e}")))?
            .next()
            .ok_or_else(|| AgentError::new(Code::Usage, format!("no address for {host}")))?;
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs_f64(timeout.min(30.0)))
            .map_err(|e| AgentError::new(Code::Precondition, format!("connect {addr}: {e}")))?;
        let _ = s.set_nodelay(true);
        let t0 = Instant::now();
        let n = if reverse { sink(&mut s, None)? } else { source(&mut s, bytes)? };
        Ok(result(if reverse { "sink" } else { "source" }, addr.to_string(), n, t0.elapsed().as_secs_f64()))
    }
}
