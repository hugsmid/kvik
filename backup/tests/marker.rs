//! Properties of the last-uploaded marker (`format.md` A.17; Q-614): the summary built record by record reads what
//! `plan::records_to_read` reads over the same records, every summary of a record list round-trips through the
//! format, no bytes make decoding panic, and the latest marker is found whatever the listing shows and however it
//! pages.

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::Arc;

use kvik_backup::fake::{MemSink, block_on};
use kvik_backup::keys::Keys;
use kvik_backup::marker::{MARKER_LEN, Marker, Summary, latest};
use kvik_backup::plan::records_to_read;
use kvik_files::record::{Body, Rebuild, Record};
use kvik_sink_api::{BackupSink, ListPage, SinkFuture, Stored};
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

/// A bucket whose listings leave out `hidden` and return at most `page` keys at a time.
struct Paged {
    inner: MemSink,
    hidden: BTreeSet<String>,
    page: usize,
}

impl BackupSink for Paged {
    fn put<'a>(&'a self, key: &'a str, b: Arc<[u8]>, sha: [u8; 32]) -> SinkFuture<'a, Stored> {
        self.inner.put(key, b, sha)
    }
    fn stat<'a>(&'a self, key: &'a str) -> SinkFuture<'a, Option<Stored>> {
        self.inner.stat(key)
    }
    fn get<'a>(&'a self, key: &'a str, r: Option<Range<u64>>) -> SinkFuture<'a, Vec<u8>> {
        self.inner.get(key, r)
    }
    fn list<'a>(&'a self, prefix: &'a str, after: Option<&'a str>) -> SinkFuture<'a, ListPage> {
        let r = block_on(self.inner.list(prefix, None)).map(|p| {
            let keys: Vec<String> = p
                .keys
                .into_iter()
                .filter(|k| !self.hidden.contains(k) && after.is_none_or(|a| k.as_str() > a))
                .collect();
            let more = keys.len() > self.page;
            ListPage::new(keys.into_iter().take(self.page).collect(), more)
        });
        Box::pin(async move { r })
    }
}

const ID: [u8; 16] = [3; 16];

/// `n` markers of generation 1 (their content does not matter to `latest`), and one of generation 2.
fn markers(n: u64, hidden: impl Fn(u64) -> bool, page: usize) -> Paged {
    let keys = Keys::new(ID);
    let inner = MemSink::new();
    for s in 1..=n {
        inner.set(&keys.marker(1, s), b"m");
    }
    inner.set(&keys.marker(2, n + 5), b"m");
    let hidden = (1..=n)
        .filter(|s| hidden(*s))
        .map(|s| keys.marker(1, s))
        .collect();
    Paged {
        inner,
        hidden,
        page,
    }
}

#[test]
fn the_latest_of_many_markers_is_found_across_listing_pages_of_a_thousand() {
    let keys = Keys::new(ID);
    for (n, hide_from) in [(2_500, u64::MAX), (2_500, 1_001), (1_000, 1), (1_001, 990)] {
        let sink = markers(n, |s| s >= hide_from, 1_000);
        assert_eq!(block_on(latest(&sink, &keys, 1)), Ok(n), "{n} {hide_from}");
    }
    assert_eq!(
        block_on(latest(&markers(0, |_| false, 1_000), &keys, 1)),
        Ok(0)
    );
}

proptest! {
    #[test]
    fn the_latest_marker_is_found_whatever_the_listing_hides_and_however_it_pages(
        n in 0u64..300,
        hidden in prop::collection::vec(any::<bool>(), 300),
        page in 1usize..20,
    ) {
        let sink = markers(n, |s| hidden[s as usize - 1], page);
        prop_assert_eq!(block_on(latest(&sink, &Keys::new(ID), 1)), Ok(n));
    }

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
