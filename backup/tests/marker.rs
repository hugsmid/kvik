//! Properties of the last-uploaded marker (`format.md` A.17; Q-614): the summary built record by record reads what
//! `plan::records_to_read` reads over the same records, every summary of a record list round-trips through the
//! format, and no bytes make decoding panic.

use kvik_backup::marker::{MARKER_LEN, Marker, Summary};
use kvik_backup::plan::records_to_read;
use kvik_files::record::{Body, Rebuild, Record};
use proptest::prelude::*;

/// What one record of a generation's uploaded list is.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Plain,
    Empty,
    Opens,
    Carried,
}

fn records() -> impl Strategy<Value = Vec<Record>> {
    let kind = prop_oneof![
        3 => Just(Kind::Plain),
        2 => Just(Kind::Empty),
        1 => Just(Kind::Opens),
        2 => Just(Kind::Carried),
    ];
    prop::collection::vec((1u64..5, kind), 1..40).prop_map(|v| {
        let mut epoch = 0;
        v.into_iter()
            .map(|(step, k)| {
                epoch += step;
                let r = Record::new(epoch, 0, 1, Body::Empty);
                match k {
                    Kind::Plain => Record {
                        body: Body::Rebuild(Rebuild::default()),
                        old_epoch: Some(1),
                        ..r
                    },
                    Kind::Empty => r,
                    Kind::Opens => Record {
                        body: Body::Rebuild(Rebuild::default()),
                        ..r
                    },
                    Kind::Carried => Record {
                        old_epoch: Some(1),
                        ..r
                    },
                }
            })
            .collect()
    })
}

proptest! {
    #[test]
    fn the_summary_record_by_record_reads_what_records_to_read_reads(v in records()) {
        let read = records_to_read(&v);
        let want = read.first().zip(read.last()).map(|(f, l)| (f.epoch, l.epoch));
        let s = Summary::of(&v);
        prop_assert_eq!(s.read, want);
        prop_assert_eq!(s.epoch, v.last().unwrap().epoch);
        for i in 0..v.len() {
            let mut again = Summary::of(&v[..i]);
            for r in &v[..i] {
                again.apply(r);
            }
            prop_assert_eq!(again, Summary::of(&v[..i]));
        }
    }

    #[test]
    fn every_summary_of_a_record_list_round_trips_through_the_format(
        v in records(),
        generation in any::<u32>(),
        sequence in 1u64..,
    ) {
        let m = Marker { generation, sequence, summary: Summary::of(&v) };
        let b = m.encode();
        prop_assert_eq!(b.len(), MARKER_LEN);
        prop_assert_eq!(Marker::decode(&b), Ok(m));
    }

    #[test]
    fn decoding_any_bytes_or_a_mutated_marker_never_panics(
        bytes in prop::collection::vec(any::<u8>(), 0..80),
        v in records(),
        at in 0usize..MARKER_LEN,
        bit in 0u8..8,
    ) {
        let _ = Marker::decode(&bytes);
        let mut b = Marker { generation: 1, sequence: 1, summary: Summary::of(&v) }.encode();
        b[at] ^= 1 << bit;
        prop_assert!(Marker::decode(&b).is_err());
    }
}
