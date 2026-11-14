//! Percentiles over recorded durations.

/// Nearest-rank percentile of `sorted` (ascending); `p` in `0.0..=100.0`. Zero for an empty slice.
pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    // The small offset keeps a product such as 99.9 % of 1000 (999.000…01 in binary) at rank 999.
    let rank = (p / 100.0 * sorted.len() as f64 - 1e-9).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// p50, p99, p99.9 and maximum of `values` (sorted in place), in nanoseconds.
pub fn summary(values: &mut [u64]) -> [u64; 4] {
    values.sort_unstable();
    [
        percentile(values, 50.0),
        percentile(values, 99.0),
        percentile(values, 99.9),
        values.last().copied().unwrap_or(0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank() {
        let v: Vec<u64> = (1..=1000).collect();
        assert_eq!(percentile(&v, 50.0), 500);
        assert_eq!(percentile(&v, 99.0), 990);
        assert_eq!(percentile(&v, 99.9), 999);
        assert_eq!(percentile(&v, 100.0), 1000);
        assert_eq!(percentile(&[], 50.0), 0);
    }
}
