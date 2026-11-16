use std::ops::Bound;
use std::time::Duration;

use kvik_log::content::BatchContent;
use kvik_log::facts::{FactFlags, ValueType as LogType};
use kvik_log::reader::ReadEvent;
use proptest::prelude::*;

use super::*;
use crate::durable::index::{Index, Stored};
use crate::durable::registry::definition_of;
use crate::series::{ChangedAtKind, Kind, SeriesSpec, ValueType};

const S: i64 = 1_000_000_000;
const T0: i64 = 1_767_225_600 * S;

fn at(sequence: u64) -> LogPos {
    LogPos {
        generation: 0,
        sequence,
    }
}

/// Series 1 to 3 defined, 3 versioned; series 4 has facts and no definition (orphans).
fn definitions() -> Vec<Item> {
    let gauge = SeriesSpec::new(
        "a/b/c",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    );
    let versioned = gauge.clone().versioned(ChangedAtKind::Version);
    [(1, gauge.clone()), (2, gauge), (3, versioned)]
        .into_iter()
        .map(|(id, spec)| {
            let d = definition_of(&spec, id, Duration::from_secs(300)).unwrap();
            Item::Entry(Entry::Definition(Box::new(d)))
        })
        .collect()
}

#[derive(Clone, Debug)]
struct Gen {
    series: u32,
    dt: i64,
    quality: u8,
    absent: bool,
    future: bool,
    changed_at: u64,
}

fn gen_fact() -> impl Strategy<Value = Gen> {
    (
        1u32..=4,
        -30i64..30,
        0u8..6,
        any::<bool>(),
        any::<bool>(),
        0u64..4,
    )
        .prop_map(|(series, dt, quality, absent, future, changed_at)| Gen {
            series,
            dt,
            quality,
            absent: absent && quality == 0,
            future,
            changed_at,
        })
}

/// Batches one second apart, each `received_at` its second; event times within half a minute either side, so some
/// facts are ahead of the write time.
fn history(batches: &[Vec<Gen>]) -> Vec<ReadBatch> {
    let mut out = Vec::new();
    let mut seq = 1u64;
    let mut push = |items: Vec<Item>, k: i64| {
        let n: u64 = items.iter().map(Item::sequences).sum();
        out.push(ReadBatch {
            file_id: 1,
            offset: 0,
            first: at(seq),
            last: at(seq + n - 1),
            commit_time: T0 + k * S,
            content: BatchContent {
                first_sequence: seq,
                received_at: T0 + k * S,
                items,
            },
        });
        seq += n;
    };
    push(definitions(), 0);
    for (k, b) in batches.iter().enumerate() {
        let k = k as i64 + 1;
        let items = b
            .iter()
            .map(|g| {
                let quality = match g.quality {
                    0 => Quality::Bad,
                    1 => Quality::Suspect,
                    _ => Quality::Good,
                };
                Item::Fact(FactRow {
                    series_id: g.series,
                    t: T0 + (k + g.dt) * S,
                    quality,
                    flags: FactFlags {
                        future: g.future,
                        ..FactFlags::default()
                    },
                    value: if g.absent {
                        Value::Absent {
                            value_type: LogType::F64,
                            scale: 0,
                        }
                    } else {
                        Value::F64((k as u64) << 8 | u64::from(g.series))
                    },
                    changed_at: (g.series == 3).then_some(g.changed_at),
                })
            })
            .collect::<Vec<_>>();
        if !items.is_empty() {
            push(items, k);
        }
    }
    out
}

fn index_of(h: &[ReadBatch], through: LogPos) -> Index {
    let mut i = Index::default();
    let events: Vec<_> = h
        .iter()
        .filter(|b| b.last <= through)
        .cloned()
        .map(ReadEvent::Batch)
        .collect();
    i.apply(&events);
    i
}

fn bad(s: &Stored) -> bool {
    s.row.quality == Quality::Bad || s.no_value()
}

/// Checks a fold's entries against the full index of the same batches; returns how many candidates were sure.
fn check(entries: &[SeriesEntry], index: &Index, written_at: i64) -> usize {
    let mut sure = 0;
    for id in 1..=4u32 {
        let e = entries.iter().find(|e| e.series_id == id);
        let facts = index.series_facts(id).count() as u64;
        assert_eq!(e.map_or(0, |e| e.fact_count), facts, "series {id}");
        if facts == 0 {
            continue;
        }
        let e = e.unwrap();
        assert_eq!(e.latest_t, index.latest_t(id), "series {id}");
        assert_eq!(
            e.lateness_reference,
            index.lateness_reference(id),
            "series {id}"
        );
        let defined = index.registry().get(id).is_some();
        assert_eq!(
            e.orphan_count,
            if defined { 0 } else { facts },
            "series {id}"
        );
        let versioned = id == 3;
        if !e.candidate_unsure {
            sure += 1;
            let expected = index
                .current_between(
                    id,
                    versioned,
                    Bound::Unbounded,
                    Bound::Included(written_at),
                    true,
                )
                .find(|s| !bad(s))
                .map(|s| (s.row.t, s.position));
            assert_eq!(
                e.candidate.map(|c| (c.t, c.position)),
                expected,
                "series {id}"
            );
        }
        if !e.pending_more {
            let expected = index
                .current_between(
                    id,
                    versioned,
                    Bound::Excluded(written_at),
                    Bound::Unbounded,
                    false,
                )
                .find(|s| !bad(s))
                .map(|s| (s.row.t, s.position));
            assert_eq!(
                e.pending.map(|p| (p.t, p.position)),
                expected,
                "series {id}"
            );
        }
    }
    sure
}

fn histories() -> impl Strategy<Value = Vec<Vec<Gen>>> {
    prop::collection::vec(prop::collection::vec(gen_fact(), 0..6), 1..40)
}

proptest! {
    /// The bounded fold says what a full index of the same batches says: counts, latest `t`, lateness reference
    /// and orphans always; the live candidate and the next pending fact whenever it does not mark them unsure.
    #[test]
    fn the_fold_equals_a_full_index(h in histories(), later in 0i64..60) {
        let h = history(&h);
        let last = h.last().unwrap().last;
        let fold = SeriesFold::new(last);
        for b in &h {
            fold.batch(b);
        }
        let written_at = h.last().unwrap().content.received_at + later * S;
        let (through, entries) = fold.entries(last, written_at).unwrap();
        prop_assert_eq!(through, last);
        check(&entries, &index_of(&h, last), written_at);
    }

    /// A fold continued from a series directory written part-way (rebuild forward from the previous generation)
    /// ends where the fold of every batch ends.
    #[test]
    fn a_fold_continued_from_a_directory_equals_the_whole_fold(h in histories(), cut in 0usize..40) {
        let h = history(&h);
        let last = h.last().unwrap().last;
        let cut = cut.min(h.len() - 1);
        let first = SeriesFold::new(last);
        for b in &h[..=cut] {
            first.batch(b);
        }
        let w0 = h[cut].content.received_at;
        let (p0, e0) = first.entries(h[cut].last, w0).unwrap();
        let continued = SeriesFold::new(last);
        continued.seed(p0, w0, &e0);
        for b in &h {
            continued.batch(b);
        }
        let written_at = h.last().unwrap().content.received_at + 5 * S;
        let (through, entries) = continued.entries(last, written_at).unwrap();
        prop_assert_eq!(through, last);
        check(&entries, &index_of(&h, last), written_at);
    }

    /// Batches above the synced mark wait: the entries are as of the last batch at or below the limit given.
    #[test]
    fn batches_above_the_synced_mark_wait(h in histories(), mid in 0usize..40) {
        let h = history(&h);
        let mid = h[mid.min(h.len() - 1)].last;
        let fold = SeriesFold::new(mid);
        for b in &h {
            fold.batch(b);
        }
        let written_at = h.last().unwrap().content.received_at;
        let (through, entries) = fold.entries(mid, written_at).unwrap();
        prop_assert_eq!(through, mid);
        check(&entries, &index_of(&h, mid), written_at);
        let last = h.last().unwrap().last;
        let (through, entries) = fold.entries(last, written_at).unwrap();
        prop_assert_eq!(through, last);
        check(&entries, &index_of(&h, last), written_at);
    }
}

#[test]
fn most_candidates_are_sure() {
    let gens: Vec<Vec<Gen>> = (0..30)
        .map(|k| {
            (1..=4)
                .map(|s| Gen {
                    series: s,
                    dt: if k % 7 == 3 { 5 } else { 0 },
                    quality: (k % 5) as u8 + 1,
                    absent: false,
                    future: false,
                    changed_at: k as u64,
                })
                .collect()
        })
        .collect();
    let h = history(&gens);
    let last = h.last().unwrap().last;
    let fold = SeriesFold::new(last);
    for b in &h {
        fold.batch(b);
    }
    let written_at = T0 + 31 * S;
    let (_, entries) = fold.entries(last, written_at).unwrap();
    assert_eq!(check(&entries, &index_of(&h, last), written_at), 4);
}

#[test]
fn a_void_below_what_was_folded_stops_the_entries() {
    let h = history(&[vec![
        Gen {
            series: 1,
            dt: 0,
            quality: 3,
            absent: false,
            future: false,
            changed_at: 0,
        };
        3
    ]]);
    let fold = SeriesFold::new(h.last().unwrap().last);
    for b in &h {
        fold.batch(b);
    }
    fold.void(at(1));
    assert_eq!(fold.entries(at(10), T0), None);
}
