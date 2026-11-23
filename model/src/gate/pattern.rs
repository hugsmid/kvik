//! Name patterns (spec §4 Funnel): three `/`-separated segments, each a literal or `*` alone.

/// Whether the grammar accepts `pattern`.
pub fn valid(pattern: &str) -> bool {
    let segs: Vec<&str> = pattern.split('/').collect();
    pattern.len() <= 255
        && segs.len() == 3
        && segs
            .iter()
            .all(|s| !s.is_empty() && (*s == "*" || !s.contains('*')))
}

/// Whether `name` matches `pattern`, segment by segment.
pub fn matches(pattern: &str, name: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let n: Vec<&str> = name.split('/').collect();
    p.len() == n.len() && p.iter().zip(&n).all(|(p, n)| *p == "*" || p == n)
}
