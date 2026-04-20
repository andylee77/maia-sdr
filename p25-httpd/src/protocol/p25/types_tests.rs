//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.


use super::*;

#[test]
fn body_status_pattern_matches_tsdu() {
    // TSDU body has 4 status dibits at {13, 49, 85, 121} per the
    // existing Phase 6F.2i validation. Verify the universal helper
    // returns the same pattern.
    let tsdu_status: Vec<usize> = (0..123)
        .filter(|p| is_body_status_dibit(*p))
        .collect();
    assert_eq!(tsdu_status, vec![13, 49, 85, 121]);
}

#[test]
fn body_status_count_matches_sdrtrunk_table() {
    // Cross-check the universal status pattern against the SDRTrunk
    // P25P1DataUnitID statusDibits field minus the in-NID status (1).
    let cases = [
        (DataUnit::Hdu, 339, 10),
        (DataUnit::Tdu, 15, 1),
        (DataUnit::Ldu1, 807, 23),
        (DataUnit::Tsdu, 123, 4),
        (DataUnit::Ldu2, 807, 23),
        (DataUnit::TduLc, 159, 5),
    ];
    for (du, expected_len, expected_n_status) in cases {
        assert_eq!(
            du.length_dibits(),
            expected_len,
            "{:?} length_dibits", du
        );
        let n_status = (0..expected_len)
            .filter(|p| is_body_status_dibit(*p))
            .count();
        assert_eq!(
            n_status, expected_n_status,
            "{:?} body status count", du
        );
        assert_eq!(
            du.data_dibits(),
            expected_len - expected_n_status,
            "{:?} data_dibits = length - n_status", du
        );
    }
}
