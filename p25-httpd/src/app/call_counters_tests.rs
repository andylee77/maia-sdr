//! Host tests for `app::call_counters` (change 057). Attached via
//! `#[cfg(test)] #[path = "call_counters_tests.rs"] mod tests;`.

use super::*;

#[test]
fn counts_are_kept_per_call_id() {
    let book = CallCounterBook::new(8);
    book.update(7, |c| c.ldu1 += 1);
    book.update(8, |c| c.ldu1 += 2);
    book.update(7, |c| c.imbe_extracted += 9);
    let a = book.get(7).unwrap();
    assert_eq!((a.ldu1, a.imbe_extracted), (1, 9));
    assert_eq!(book.get(8).unwrap().ldu1, 2);
    assert!(book.get(9).is_none());
    // call_id 0 = no call known: never recorded.
    assert!(book.update(0, |c| c.ldu1 += 1).is_none());
    assert!(book.get(0).is_none());
    assert_eq!(book.len(), 2);
}

#[test]
fn oldest_calls_are_dropped_beyond_capacity() {
    let book = CallCounterBook::new(3);
    for id in 1..=5u64 {
        book.update(id, |c| c.hdu += id);
    }
    assert_eq!(book.len(), 3);
    assert!(book.get(1).is_none() && book.get(2).is_none());
    assert_eq!(book.get(5).unwrap().hdu, 5);
    // Updating an existing (old) entry does not evict anything.
    book.update(3, |c| c.hdu += 1);
    assert_eq!(book.get(3).unwrap().hdu, 4);
    assert_eq!(book.len(), 3);
}

#[test]
fn end_marker_needs_voice_since_the_previous_one() {
    let mut c = CallCounts::default();
    // TDULC before any voice (the previous transmission's hang, decoded
    // under the new call after a re-grant): not an end of this call.
    assert!(!c.note_end_marker());
    c.ldu1 = 4;
    c.ldu2 = 4;
    assert!(c.note_end_marker());
    // The hang repeats TDULCs: one end, not many.
    assert!(!c.note_end_marker());
    assert!(!c.note_end_marker());
    // Voice resumed on the same call (re-key without a new grant).
    c.ldu1 += 1;
    assert!(c.note_end_marker());
    assert_eq!(c.end_markers, 2);
}

#[test]
fn concurrent_writers_do_not_lose_counts() {
    let book = std::sync::Arc::new(CallCounterBook::default());
    let mut th = Vec::new();
    for t in 0..4u64 {
        let b = book.clone();
        th.push(std::thread::spawn(move || {
            for _ in 0..1_000 {
                b.update(100 + (t % 2), |c| c.vocoder_pcm_samples += 160);
            }
        }));
    }
    for t in th {
        t.join().unwrap();
    }
    assert_eq!(book.get(100).unwrap().vocoder_pcm_samples, 2 * 1_000 * 160);
    assert_eq!(book.get(101).unwrap().vocoder_pcm_samples, 2 * 1_000 * 160);
}
