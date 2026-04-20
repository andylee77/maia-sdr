//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.

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
