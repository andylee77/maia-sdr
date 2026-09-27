//! Latency statistics: log2 histogram and percentiles (pure).

use serde_json::{json, Value};

/// Percentile (nearest-rank) of an unsorted sample; `q` in [0, 100].
pub fn percentile(samples: &[u64], q: f64) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut v = samples.to_vec();
    v.sort_unstable();
    let rank = ((q / 100.0) * v.len() as f64).ceil() as usize;
    Some(v[rank.clamp(1, v.len()) - 1])
}

/// Histogram with bins [0,1), [1,2), [2,4), [4,8), ... (value units, e.g. us).
pub fn log2_hist(samples: &[u64]) -> Vec<(u64, u64)> {
    let mut bins = [0u64; 65];
    for &s in samples {
        let b = if s == 0 { 0 } else { 64 - s.leading_zeros() as usize };
        bins[b] += 1;
    }
    let last = bins.iter().rposition(|c| *c > 0).unwrap_or(0);
    (0..=last)
        .map(|b| (if b == 0 { 0 } else { 1u64 << (b - 1) }, bins[b]))
        .collect()
}

pub fn summary(samples: &[u64], unit: &str) -> Value {
    let n = samples.len();
    let sum: u128 = samples.iter().map(|x| *x as u128).sum();
    json!({
        "unit": unit,
        "count": n,
        "min": samples.iter().min(),
        "p50": percentile(samples, 50.0),
        "p90": percentile(samples, 90.0),
        "p99": percentile(samples, 99.0),
        "p999": percentile(samples, 99.9),
        "max": samples.iter().max(),
        "mean": if n > 0 { Some(crate::util::round1(sum as f64 / n as f64)) } else { None },
        "hist_log2": log2_hist(samples).iter().map(|(lo, c)| json!({"ge": lo, "count": c})).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), Some(50));
        assert_eq!(percentile(&v, 99.0), Some(99));
        assert_eq!(percentile(&v, 100.0), Some(100));
        assert_eq!(percentile(&v, 0.0), Some(1));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn hist() {
        let h = log2_hist(&[0, 1, 2, 3, 4, 1000]);
        assert_eq!(h[0], (0, 1));
        assert_eq!(h[1], (1, 1));
        assert_eq!(h[2], (2, 2));
        assert_eq!(h[3], (4, 1));
        assert_eq!(h.last().unwrap(), &(512, 1));
    }
}
