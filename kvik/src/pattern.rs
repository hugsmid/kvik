//! Name patterns (spec §4 Funnel, Routing; Q-88): the three `/`-separated segments of a name, each a literal or `*`,
//! which matches one whole segment. [`check`] is the grammar, applied when routes are compiled. [`matches`] and
//! [`overlap`] are total over any string, so a pattern the grammar refuses that a log already holds is read as built:
//! each segment a literal, `*` alone the only wildcard (`format.md` A.6 field 55, Q-670 (b)).

use crate::error::RouteError;

/// `format.md` A.6 field 55.
const MAX_LEN: usize = 255;
const SEGMENTS: usize = 3;
const ANY: &str = "*";

/// Whether `pattern` is in the grammar, the reason it is refused if not.
pub(crate) fn check(pattern: &str) -> Result<(), RouteError> {
    if pattern.len() > MAX_LEN {
        return Err(RouteError::TooLong { len: pattern.len() });
    }
    let found = pattern.split('/').count();
    if found != SEGMENTS {
        return Err(RouteError::Segments { found });
    }
    for s in pattern.split('/') {
        if s.is_empty() {
            return Err(RouteError::EmptySegment);
        }
        if s != ANY && s.contains('*') {
            return Err(RouteError::PartialWildcard);
        }
    }
    Ok(())
}

/// Whether `name` matches `pattern`: as many segments, each equal or under `*`.
pub(crate) fn matches(pattern: &str, name: &str) -> bool {
    per_segment(pattern, name, |p, n| p == ANY || p == n)
}

/// Whether one name could match both patterns: as many segments, each equal or `*` in either.
pub(crate) fn overlap(p: &str, q: &str) -> bool {
    per_segment(p, q, |a, b| a == ANY || b == ANY || a == b)
}

fn per_segment(a: &str, b: &str, meet: impl Fn(&str, &str) -> bool) -> bool {
    let (mut a, mut b) = (a.split('/'), b.split('/'));
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if meet(x, y) => {}
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_segments_of_literals_or_a_whole_wildcard_are_accepted() {
        for p in [
            "a/b/c",
            "*/*/*",
            "plant/*/power",
            "*/line-1/*",
            "a b/c.d/e-f",
        ] {
            assert_eq!(check(p), Ok(()), "{p}");
        }
    }

    #[test]
    fn a_pattern_without_three_segments_is_refused() {
        for (p, found) in [
            ("", 1),
            ("*", 1),
            ("plant/*", 2),
            ("a/b/c/d", 4),
            ("plant/*/power/", 4),
            ("/a/b/c", 4),
        ] {
            assert_eq!(check(p), Err(RouteError::Segments { found }), "{p}");
        }
    }

    #[test]
    fn an_empty_segment_is_refused() {
        for p in ["/b/c", "a//c", "a/b/", "//", "*//*"] {
            assert_eq!(check(p), Err(RouteError::EmptySegment), "{p}");
        }
    }

    #[test]
    fn a_wildcard_inside_a_segment_is_refused() {
        for p in [
            "plant/line-*/power",
            "a*/b/c",
            "a/*b/c",
            "a/b/**",
            "a/b/*c*",
            "**/b/c",
        ] {
            assert_eq!(check(p), Err(RouteError::PartialWildcard), "{p}");
        }
    }

    #[test]
    fn a_pattern_over_255_bytes_is_refused_and_one_of_255_accepted() {
        let seg = "x".repeat(84);
        let p = format!("{seg}/{seg}/{seg}y");
        assert_eq!(p.len(), 255);
        assert_eq!(check(&p), Ok(()));
        let q = format!("{p}z");
        assert_eq!(check(&q), Err(RouteError::TooLong { len: 256 }));
    }

    #[test]
    fn the_length_limit_counts_bytes_not_characters() {
        let e = |n: usize| "é".repeat(n);
        let p = format!("{}/{}/{}x", e(42), e(42), e(42));
        assert_eq!((p.len(), p.chars().count()), (255, 129));
        assert_eq!(check(&p), Ok(()));
        let q = format!("{}/{}/{}", e(42), e(42), e(43));
        assert_eq!((q.len(), q.chars().count()), (256, 129));
        assert_eq!(check(&q), Err(RouteError::TooLong { len: 256 }));
    }

    #[test]
    fn a_wildcard_matches_one_whole_segment() {
        assert!(matches("plant/*/power", "plant/line-1/power"));
        assert!(matches("plant/*/power", "plant//power"));
        assert!(matches("*/*/*", "a/b/c"));
        assert!(matches("a/b/c", "a/b/c"));
        assert!(!matches("plant/*/power", "plant/line-1/power-smoothed"));
        assert!(!matches("plant/*/power", "plant/line-1/sub/power"));
        assert!(!matches("plant/*/power", "plant/power"));
        assert!(!matches("a/b/c", "a/b/C"));
    }

    #[test]
    fn a_star_in_a_name_is_matched_only_by_the_wildcard() {
        assert!(matches("a/*/c", "a/*/c"));
        assert!(matches("a/*/c", "a/b*/c"));
        assert!(!matches("a/b/c", "a/*/c"));
    }

    #[test]
    fn a_refused_pattern_is_matched_literally() {
        assert!(!matches("plant/line-*/power", "plant/line-1/power"));
        assert!(matches("plant/line-*/power", "plant/line-*/power"));
        assert!(matches("*/b", "a/b"));
    }

    #[test]
    fn overlap_table() {
        let table = [
            ("a/b/c", "a/b/c", true),
            ("a/b/c", "a/b/d", false),
            ("plant/*/power", "plant/line-1/power", true),
            ("plant/*/power", "*/line-1/*", true),
            ("plant/*/power", "plant/*/energy", false),
            ("plant/*/power", "*/*/*", true),
            ("*/x/*", "*/y/*", false),
            ("a/*/*", "*/b/*", true),
            ("a/*/c", "b/*/*", false),
            ("plant/*/power", "plant/*/power-smoothed", false),
            ("plant/*/power", "plant/*", false),
            ("*/*/*", "*/*", false),
        ];
        for (p, q, want) in table {
            assert_eq!(overlap(p, q), want, "{p} {q}");
            assert_eq!(overlap(q, p), want, "{q} {p}");
        }
    }
}
