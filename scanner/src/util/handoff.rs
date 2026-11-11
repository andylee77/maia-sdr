//! Handing a response's chunks from the thread that makes them to the task that sends them, with
//! a bound on how long a client may take none: a client that stalls without leaving would
//! otherwise hold its thread, and whatever that thread holds, for good.

use std::time::{Duration, Instant};

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::Sender;

/// A client that takes nothing for this long is gone.
pub const STALL: Duration = Duration::from_secs(60);
/// How often a full queue is looked at again.
const LOOK: Duration = Duration::from_millis(5);

/// Hand `item` to the queue, waiting while it is full. False when the client left, or took
/// nothing for `stall`: the maker stops then.
pub fn hand<T>(tx: &Sender<T>, mut item: T, stall: Duration) -> bool {
    let deadline = Instant::now() + stall;
    loop {
        match tx.try_send(item) {
            Ok(()) => return true,
            Err(TrySendError::Closed(_)) => return false,
            Err(TrySendError::Full(back)) => {
                if Instant::now() >= deadline {
                    return false;
                }
                item = back;
                std::thread::sleep(LOOK);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queue_that_is_read_takes_everything() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(2);
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            while let Some(n) = rx.blocking_recv() {
                std::thread::sleep(Duration::from_millis(2));
                got.push(n);
            }
            got
        });
        for n in 0..20 {
            assert!(hand(&tx, n, Duration::from_secs(5)));
        }
        drop(tx);
        assert_eq!(reader.join().unwrap(), (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn a_client_that_stalls_or_leaves_ends_the_maker() {
        let (tx, rx) = tokio::sync::mpsc::channel::<u32>(1);
        assert!(hand(&tx, 1, Duration::from_millis(50)));
        // The queue is full and nobody reads it.
        let started = Instant::now();
        assert!(!hand(&tx, 2, Duration::from_millis(50)));
        assert!(started.elapsed() >= Duration::from_millis(50) && started.elapsed() < Duration::from_secs(2));
        // The client gone.
        drop(rx);
        assert!(!hand(&tx, 3, Duration::from_secs(5)));
    }
}
