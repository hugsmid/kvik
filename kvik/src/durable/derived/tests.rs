use std::ops::Bound;
use std::time::Duration;

use kvik_log::content::BatchContent;
use kvik_log::directory::series::CandidateValue;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value, ValueType as LogType};
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
            Item::Entry(Box::new(Entry::Definition(Box::new(d))))
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
    unsynced: bool,
    kind: u8,
}

fn gen_fact() -> impl Strategy<Value = Gen> {
    (
        (1u32..=4, -30i64..30, 0u8..6),
        (any::<bool>(), any::<bool>(), 0u64..4, any::<bool>(), 0u8..5),
    )
        .prop_map(
            |((series, dt, quality), (absent, future, changed_at, unsynced, kind))| Gen {
                series,
                dt,
                quality,
                absent: absent && quality == 0,
                future,
                changed_at,
                unsynced,
                kind,
            },
        )
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
                        unsynced_clock: g.unsynced,
                        ..FactFlags::default()
                    },
                    value: if g.absent {
                        Value::Absent {
                            value_type: LogType::F64,
                            scale: 0,
                        }
                    } else {
                        let v = (k as u64) << 8 | u64::from(g.series);
                        match g.kind {
                            0 => Value::F64(v),
                            1 => Value::Decimal {
                                units: -(v as i64),
                                scale: (k % 19) as u8,
                            },
                            2 => Value::State(v as u16),
                            3 => Value::Bool(k % 2 == 0),
                            _ => Value::Text(format!("t{v}")),
                        }
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
    i.apply(&events, through);
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
            // Value and quality as the fact holds them, unless the entry says to read them at the position.
            if let Some(c) = e
                .candidate
                .filter(|c| c.value != CandidateValue::AtPosition)
            {
                let f = index
                    .series_facts(id)
                    .find(|s| s.position == c.position)
                    .unwrap();
                let expected = match &f.row.value {
                    Value::F64(b) => CandidateValue::F64(*b),
                    Value::Decimal { units, scale } => CandidateValue::Decimal {
                        units: *units,
                        scale: *scale,
                    },
                    Value::State(x) => CandidateValue::State(*x),
                    Value::Bool(x) => CandidateValue::Bool(*x),
                    Value::Text(_) => CandidateValue::Text,
                    Value::Absent { .. } => unreachable!("a candidate has a value"),
                };
                assert_eq!(c.value, expected, "series {id}");
                assert_eq!(c.quality, f.row.quality, "series {id}");
            }
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
        let e = fold.entries(last, written_at).unwrap();
        prop_assert_eq!(e.through, last);
        prop_assert!(e.judged_at >= written_at);
        check(&e.entries, &index_of(&h, last), e.judged_at);
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
        let e0 = first.entries(h[cut].last, w0).unwrap();
        let continued = SeriesFold::new(last);
        continued.seed(e0.through, e0.judged_at, &e0.entries);
        for b in &h {
            continued.batch(b);
        }
        let written_at = h.last().unwrap().content.received_at + 5 * S;
        let e = continued.entries(last, written_at).unwrap();
        prop_assert_eq!(e.through, last);
        check(&e.entries, &index_of(&h, last), e.judged_at);
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
        let e = fold.entries(mid, written_at).unwrap();
        prop_assert_eq!(e.through, mid);
        check(&e.entries, &index_of(&h, mid), e.judged_at);
        let last = h.last().unwrap().last;
        let e = fold.entries(last, written_at).unwrap();
        prop_assert_eq!(e.through, last);
        check(&e.entries, &index_of(&h, last), e.judged_at);
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
                    unsynced: false,
                    kind: 0,
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
    let e = fold.entries(last, written_at).unwrap();
    assert_eq!(check(&e.entries, &index_of(&h, last), e.judged_at), 4);
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
            unsynced: false,
            kind: 0,
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

#[test]
fn a_pending_fact_that_falls_due_after_a_continuation_is_read_at_its_position() {
    let g = |dt, quality| Gen {
        series: 1,
        dt,
        quality,
        absent: false,
        future: true,
        changed_at: 0,
        unsynced: false,
        kind: 0,
    };
    // A suspect fact 20 s ahead, then an ordinary fact a second later; a continuation; then 60 s.
    let h = history(&[vec![g(20, 1)], vec![g(0, 3)]]);
    let last = h.last().unwrap().last;
    let whole = SeriesFold::new(last);
    for b in &h {
        whole.batch(b);
    }
    let w0 = h.last().unwrap().content.received_at;
    let e0 = whole.entries(last, w0).unwrap();
    let continued = SeriesFold::new(last);
    continued.seed(e0.through, e0.judged_at, &e0.entries);
    let later = w0 + 60 * S;
    let a = whole.entries(last, later).unwrap().entries[0];
    let b = continued.entries(last, later).unwrap().entries[0];
    let (ca, cb) = (a.candidate.unwrap(), b.candidate.unwrap());
    assert_eq!((ca.t, ca.position), (cb.t, cb.position));
    assert_eq!(ca.quality, Quality::Suspect);
    assert_eq!(
        cb.value,
        CandidateValue::AtPosition,
        "the entry held no value for it"
    );
}

#[test]
fn the_fold_follows_the_synced_mark_without_being_asked_for_entries() {
    let gens: Vec<Vec<Gen>> = (0..10)
        .map(|_| {
            vec![Gen {
                series: 1,
                dt: 0,
                quality: 3,
                absent: false,
                future: false,
                changed_at: 0,
                unsynced: false,
                kind: 0,
            }]
        })
        .collect();
    let h = history(&gens);
    let fold = SeriesFold::new(LogPos::default());
    let mark = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let m = mark.clone();
    fold.follow(move || LogPos {
        generation: 0,
        sequence: m.load(std::sync::atomic::Ordering::SeqCst),
    });
    for b in &h {
        mark.store(b.last.sequence, std::sync::atomic::Ordering::SeqCst);
        fold.batch(b);
        assert!(
            lock(&fold.fold).queued.is_empty(),
            "a batch at or below the mark waits"
        );
    }
    assert_eq!(lock(&fold.fold).applied, h.last().unwrap().last);
}

/// Series 3 (versioned) facts at `t` one second apart from `T0 + 1 s`: `(dt, quality code, changed_at)`.
fn versioned(facts: &[&[(i64, u8, u64)]]) -> Vec<ReadBatch> {
    let gens: Vec<Vec<Gen>> = facts
        .iter()
        .map(|b| {
            b.iter()
                .map(|&(dt, quality, changed_at)| Gen {
                    series: 3,
                    dt,
                    quality,
                    absent: false,
                    future: false,
                    changed_at,
                    unsynced: false,
                    kind: 0,
                })
                .collect()
        })
        .collect();
    history(&gens)
}

/// Folds `h`, continues a second fold from the first's entries after batch `cut`, and checks the continuation's
/// final entries against the full index.
fn continue_after(h: &[ReadBatch], cut: usize) -> SeriesEntry {
    let last = h.last().unwrap().last;
    let first = SeriesFold::new(last);
    for b in &h[..=cut] {
        first.batch(b);
    }
    let e0 = first
        .entries(h[cut].last, h[cut].content.received_at)
        .unwrap();
    let continued = SeriesFold::new(last);
    continued.seed(e0.through, e0.judged_at, &e0.entries);
    for b in h {
        continued.batch(b);
    }
    let e = continued
        .entries(last, h.last().unwrap().content.received_at + S)
        .unwrap();
    check(&e.entries, &index_of(h, last), e.judged_at);
    *e.entries.iter().find(|e| e.series_id == 3).unwrap()
}

#[test]
fn a_continued_versioned_series_does_not_take_a_version_a_bad_one_before_the_file_outranks() {
    // Before the file: a good fact at t=2 and a bad one at t=5 of version 3. After it: version 1 of t=5, outranked.
    let h = versioned(&[&[(1, 3, 0)], &[(3, 0, 3)], &[(2, 3, 1)]]);
    let e = continue_after(&h, 2);
    assert!(e.candidate_unsure || e.candidate.is_some_and(|c| c.t == T0 + 2 * S));
}

#[test]
fn a_continued_pending_fact_of_a_versioned_series_has_no_rank_to_keep_it() {
    // Before the file: version 3 of t = 30 s, ahead. After it: version 1 of the same `t`, which it outranks.
    let h = versioned(&[&[(29, 3, 3)], &[(28, 3, 1)], &[(0, 3, 0)]]);
    let e = continue_after(&h, 1);
    assert!(e.pending_more || e.pending.is_some_and(|p| p.position.sequence == 4));
}

#[test]
fn a_segment_damaged_or_unreadable_is_reason_other_and_degrades_the_status_file() {
    use kvik_log::writer::Reason as LogReason;
    for r in [
        LogReason::SegmentDamaged { file_id: 7 },
        LogReason::SegmentUnreadable { file_id: 7 },
    ] {
        assert_eq!(
            status_health(vec![r]),
            (health::DEGRADED, vec![reason::OTHER])
        );
    }
    assert_eq!(status_health(Vec::new()), (health::OK, Vec::new()));
}

/// The latch's find of batch `lost`, met between the batches before and after it, at byte `start` of segment 1.
fn found(h: &[ReadBatch], lost: usize, start: u64) -> Vec<crate::durable::loss::Find> {
    use kvik_log::scan::{InvalidRegion, LossCause as ScanCause};
    let region = ReadEvent::Corruption {
        file_id: 1,
        region: InvalidRegion {
            start,
            end: start + 64,
            last_sequence_before: Some(h[lost - 1].last.sequence),
            first_sequence_after: Some(h[lost + 1].first.sequence),
            cause: ScanCause::Corruption,
        },
    };
    let events = [
        ReadEvent::Batch(h[lost - 1].clone()),
        region,
        ReadEvent::Batch(h[lost + 1].clone()),
    ];
    crate::durable::loss::Finder::default().apply(&events, h.last().unwrap().last)
}

/// A loss found ahead of what the fold took, and found again once the fold has passed over it (its batch never
/// arrives), leaves the fold whole; a loss found at a batch it folded stops it (Q-952).
#[test]
fn a_loss_found_ahead_of_the_fold_leaves_it_whole_and_one_behind_it_stops_it() {
    use crate::durable::loss::Latch;
    let one = Gen {
        series: 1,
        dt: 0,
        quality: 3,
        absent: false,
        future: false,
        changed_at: 0,
        unsynced: false,
        kind: 0,
    };
    let h = history(&vec![vec![one]; 10]);
    let fold = SeriesFold::new(h.last().unwrap().last);
    let latch = Arc::new(Latch::default());
    fold.watch_losses(latch.clone());
    for b in &h[..4] {
        fold.batch(b);
    }
    latch.note(&found(&h, 6, 600));
    for (k, b) in h.iter().enumerate().skip(4) {
        if k != 6 {
            fold.batch(b);
        }
    }
    assert!(fold.entries(h.last().unwrap().last, T0).is_some());
    latch.note(&found(&h, 6, 600));
    assert!(fold.entries(h.last().unwrap().last, T0).is_some());
    latch.note(&found(&h, 2, 200));
    assert!(fold.entries(h.last().unwrap().last, T0).is_none());
}
