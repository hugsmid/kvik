//! Generated compacted segments of both classes (`docs/format-compacted.md` B.3): facts in, the same facts out with
//! positions, receive and commit times; entries; statistics equal to direct computation; canonical bytes; as-of
//! time from the commit-time tables equal to as-of time from the log's commit blocks.

mod compacted;

use compacted::*;
use kvik_chunk::file::{Batch, Class, CompactedFile, Fact, Position, Reach, Series};
use kvik_chunk::value_type;
use proptest::prelude::*;

/// The last position whose batch commit time is at or below `t`, from the log's commit blocks.
fn as_of_log(batches: &[Batch], t: i64) -> Option<Position> {
    batches
        .iter()
        .filter(|b| b.commit_time <= t)
        .map(|b| (b.generation, b.last_sequence))
        .max()
}

proptest! {
    #[test]
    fn files_give_back_their_facts_entries_positions_and_times(s in spec()) {
        let (c, _) = build(&s);
        check_round_trip(&c);
    }

    #[test]
    fn as_of_from_the_tables_equals_as_of_from_the_commit_blocks(s in spec()) {
        let (a, absent) = build(&s);
        let mut files = vec![a.clone()];
        if !absent.is_empty() {
            let mut b = Content::new(Class::Current);
            b.id.file_id = 41;
            b.batches = a.batches.clone();
            b.series.push(Series {
                series_id: 99,
                name: None,
                reach: Reach::Point,
                facts: absent
                    .iter()
                    .enumerate()
                    .map(|(i, &p)| Fact::value(99, p, T0 + i as i64, value_type::BOOL, 1))
                    .collect(),
                set_aside: vec![],
            });
            files.push(b.settle());
        }
        let written: Vec<(Vec<u8>, Content)> = files.into_iter().map(|c| (c.write().0, c)).collect();
        let opened: Vec<_> = written
            .iter()
            .map(|(bytes, c)| CompactedFile::open(bytes.as_slice(), &c.id, None).unwrap())
            .collect();
        let mut probes: Vec<i64> = a.batches.iter().flat_map(|b| [b.commit_time - 1, b.commit_time, b.commit_time + 1]).collect();
        probes.push(i64::MIN);
        for t in probes {
            let from_tables = opened.iter().map(|f| f.as_of(t).unwrap()).max().flatten();
            prop_assert_eq!(from_tables, as_of_log(&a.batches, t), "as of {}", t);
        }
    }
}
