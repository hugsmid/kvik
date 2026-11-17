//! The invalid regions this process has reported lost, as a union of byte spans per segment (Q-922).

use std::collections::BTreeMap;

/// Byte spans by segment, overlapping and touching spans merged, so that a region met later is covered when the
/// union holds it, however many finds it took to grow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Regions(BTreeMap<u64, BTreeMap<u64, u64>>);

impl Regions {
    /// Adds the span `start..end` of segment `file_id`.
    pub(crate) fn insert(&mut self, file_id: u64, start: u64, end: u64) {
        let spans = self.0.entry(file_id).or_default();
        let (mut a, mut z) = (start, end.max(start));
        let touching: Vec<u64> = spans
            .range(..=z)
            .rev()
            .take_while(|(_, e)| **e >= a)
            .map(|(s, _)| *s)
            .collect();
        for s in touching {
            let e = spans.remove(&s).expect("listed above");
            a = a.min(s);
            z = z.max(e);
        }
        spans.insert(a, z);
    }

    /// The regions no lost range of `recorded` (the manifest's, by segment and bytes) holds whole (Q-755).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn unrecorded(&self, recorded: &[kvik_log::manifest::LostRange]) -> u64 {
        let mut n = 0;
        for (file_id, spans) in &self.0 {
            for (a, z) in spans {
                let inside = recorded.iter().any(|l| {
                    l.file_id == *file_id
                        && l.offset <= *a
                        && *z <= l.offset.saturating_add(l.length)
                });
                n += u64::from(!inside);
            }
        }
        n
    }

    /// Whether the union holds the whole span `start..end` of segment `file_id`.
    pub(crate) fn covers(&self, file_id: u64, start: u64, end: u64) -> bool {
        self.0
            .get(&file_id)
            .and_then(|spans| spans.range(..=start).next_back())
            .is_some_and(|(_, e)| end <= *e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touching_and_overlapping_spans_are_one_region() {
        let mut r = Regions::default();
        r.insert(3, 100, 200);
        r.insert(3, 200, 300);
        assert!(r.covers(3, 100, 300), "touching");
        r.insert(3, 500, 600);
        assert!(!r.covers(3, 250, 550), "a gap between them");
        r.insert(3, 280, 520);
        assert!(r.covers(3, 100, 600), "a span bridging two");
        assert_eq!(r.0[&3].len(), 1);
    }

    #[test]
    fn a_span_is_covered_only_inside_the_union_of_its_segment() {
        let mut r = Regions::default();
        r.insert(3, 64, 400);
        assert!(r.covers(3, 64, 400));
        assert!(r.covers(3, 128, 256));
        assert!(!r.covers(3, 64, 401), "lengthened by damage found later");
        assert!(!r.covers(3, 32, 400));
        assert!(!r.covers(4, 64, 400));
        r.insert(3, 401, 500);
        assert!(!r.covers(3, 64, 500), "a byte apart is not touching");
        r.insert(3, 10, 20);
        assert!(!r.covers(3, 10, 100), "the span below does not reach it");
    }

    #[test]
    fn a_region_is_recorded_only_inside_a_lost_range_of_its_segment() {
        let range = |file_id, offset, length| kvik_log::manifest::LostRange {
            identity: (0, 1),
            first: (0, 1),
            last: (0, 2),
            file_id,
            offset,
            length,
            cause: kvik_log::manifest::LossCause::Corruption,
            boot_id: [0; 16],
            id_floor: 0,
            time_before: None,
            time_after: None,
            extends: false,
            stamp: None,
        };
        let mut r = Regions::default();
        r.insert(3, 64, 400);
        assert_eq!(r.unrecorded(&[]), 1);
        assert_eq!(r.unrecorded(&[range(3, 64, 336)]), 0);
        assert_eq!(
            r.unrecorded(&[range(3, 64, 335)]),
            1,
            "ends short of the region"
        );
        assert_eq!(r.unrecorded(&[range(3, 72, 400)]), 1, "starts after it");
        assert_eq!(r.unrecorded(&[range(4, 64, 336)]), 1, "another segment");
    }
}
