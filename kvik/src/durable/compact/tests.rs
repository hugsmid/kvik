mod plan;
mod run;
mod task;
mod verify;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use kvik_chunk::file::{Class, CompactedFile, Fact, Position, Reach};
use kvik_log::batch::SegmentIdentity;
use kvik_log::body::IndexRecord;
use kvik_log::content::{BatchContent, Item};
use kvik_log::entries::{Definition, Entry, Kind, Retention, Timing, Versioning};
use kvik_log::facts::{FactFlags, FactRow, Quality, Value, ValueType, flag};
use kvik_log::fs::StdFs;
use kvik_log::name::SegmentName;
use kvik_log::segment::SegmentImage;
use kvik_log::setaside::{RestoredEnd, SetAside};
use kvik_log::store::LOG_DIR;
use kvik_model::compaction::{Held, Placed, Placement, place_l0};
use proptest::prelude::*;

use super::plan::Candidate;
use super::run::{RunInput, run};
use super::{Cause, Output, RunError, Stall};

const STORE: [u8; 16] = [7; 16];
const DAY: i64 = 86_400_000_000_000;

/// One batch of a test segment.
#[derive(Clone, Debug)]
struct TestBatch {
    first: u64,
    received_at: i64,
    commit_time: i64,
    items: Vec<Item>,
}

fn temp_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("kvik-compact-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(d.join(LOG_DIR)).unwrap();
    d
}

/// The bytes of a sealed segment holding `batches`, with the segment index the sealer writes.
fn segment_bytes(generation: u32, file_id: u64, batches: &[TestBatch]) -> Vec<u8> {
    let mut img = image(generation, file_id, batches[0].first);
    let mut index: BTreeMap<u32, IndexRecord> = BTreeMap::new();
    for b in batches {
        let content = BatchContent {
            first_sequence: b.first,
            received_at: b.received_at,
            items: b.items.clone(),
        };
        img.append(&content.builder().unwrap(), b.commit_time)
            .unwrap();
        for item in &b.items {
            if let Item::Fact(r) = item {
                let rec = index.entry(r.series_id).or_insert(IndexRecord {
                    series_id: r.series_id,
                    fact_count: 0,
                    min_t: r.t,
                    max_t: r.t,
                });
                rec.fact_count += 1;
                rec.min_t = rec.min_t.min(r.t);
                rec.max_t = rec.max_t.max(r.t);
            }
        }
    }
    seal(img, index.into_values().collect())
}

fn image(generation: u32, file_id: u64, base: u64) -> SegmentImage {
    let id = SegmentIdentity {
        store_id: STORE,
        file_id,
        generation,
    };
    let mut img = SegmentImage::create_ahead(id, 1, 4 << 20).unwrap();
    img.take_into_use(base).unwrap();
    img
}

fn seal(mut img: SegmentImage, index: Vec<IndexRecord>) -> Vec<u8> {
    let end = img.write_seal_blocks(index).unwrap();
    img.set_len(end);
    img.into_bytes()
}

fn put_segment(dir: &Path, generation: u32, file_id: u64, bytes: &[u8]) -> Candidate {
    let name = SegmentName {
        generation,
        file_id,
        sealed: true,
    };
    let path = dir.join(LOG_DIR).join(name.to_string());
    std::fs::write(&path, bytes).unwrap();
    Candidate {
        generation,
        file_id,
        seal: kvik_log::directory::catalogue::read_trailer(&StdFs, &path),
        path,
        listed: true,
        range: None,
    }
}

fn input<'a>(dir: &'a Path, set_aside: &'a SetAside, weeks: &'a BTreeSet<i64>) -> RunInput<'a> {
    RunInput {
        fs: &StdFs,
        dir,
        store_id: STORE,
        generation: 1,
        first_file_id: 100,
        created_at: 5,
        set_aside,
        weeks_merged: weeks,
        registry: &|_| None,
    }
}

fn reading(series_id: u32, t: i64, value: Value) -> Item {
    Item::Fact(FactRow {
        series_id,
        t,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value,
        changed_at: None,
    })
}

fn batch(first: u64, commit_time: i64, items: Vec<Item>) -> TestBatch {
    TestBatch {
        first,
        received_at: commit_time,
        commit_time,
        items,
    }
}

/// The log row a compacted fact came from, read back field by field (`format.md` A.5).
fn log_row(f: &Fact) -> FactRow {
    let vt = ValueType::from_code(f.value_type).unwrap();
    let value = if f.flags & flag::NO_VALUE != 0 {
        Value::Absent {
            value_type: vt,
            scale: f.scale,
        }
    } else {
        match vt {
            ValueType::F64 => Value::F64(f.value),
            ValueType::Decimal => Value::Decimal {
                units: f.value as i64,
                scale: f.scale,
            },
            ValueType::State => Value::State(f.value as u16),
            ValueType::Bool => Value::Bool(f.value == 1),
            ValueType::Text => Value::Text(String::from_utf8(f.text.clone()).unwrap()),
        }
    };
    let bit = |b: u16| f.flags & b != 0;
    FactRow {
        series_id: f.series_id,
        t: f.t,
        quality: match f.quality {
            0 => Quality::Good,
            1 => Quality::Estimated,
            2 => Quality::Suspect,
            _ => Quality::Bad,
        },
        flags: FactFlags {
            future: bit(flag::FUTURE),
            late: bit(flag::LATE),
            revise: bit(flag::REVISE),
            host_time: bit(flag::HOST_TIME),
            unsynced_clock: bit(flag::UNSYNCED_CLOCK),
            rounded: bit(flag::ROUNDED),
        },
        value,
        changed_at: bit(flag::HAS_CHANGED_AT).then_some(f.changed_at),
    }
}

/// What a position holds: its item, receive time and commit time.
type Content = (Item, i64, i64);

fn input_contents(segments: &[(u32, Vec<TestBatch>)]) -> BTreeMap<Position, Content> {
    let mut out = BTreeMap::new();
    for (g, batches) in segments {
        for b in batches {
            let mut s = b.first;
            for item in &b.items {
                out.insert((*g, s), (item.clone(), b.received_at, b.commit_time));
                s += item.sequences();
            }
        }
    }
    out
}

/// Every position the outputs hold, with its content and the file it is in.
fn output_contents(outputs: &[Output]) -> BTreeMap<Position, Vec<(Content, Placement)>> {
    let mut out: BTreeMap<Position, Vec<(Content, Placement)>> = BTreeMap::new();
    for o in outputs {
        let bytes = std::fs::read(&o.path).unwrap();
        let file = CompactedFile::open(bytes.as_slice(), &o.id, Some(&o.layout)).unwrap();
        let d = file.description().clone();
        let placed = |class| Placement {
            start: d.period_start,
            len: d.period_len,
            class,
        };
        for e in file.index().unwrap() {
            let class = match (d.class, e.set_aside()) {
                (_, true) => Placed::SetAside,
                (Class::Current, _) => Placed::Current,
                (Class::Revision, _) => Placed::Revision,
            };
            for r in file.read_chunk(&e).unwrap() {
                out.entry(r.fact.position).or_default().push((
                    (Item::Fact(log_row(&r.fact)), r.received_at, r.commit_time),
                    placed(class),
                ));
            }
        }
        for r in file.entries().unwrap() {
            out.entry(r.position).or_default().push((
                (Item::Entry(Box::new(r.entry)), r.received_at, r.commit_time),
                placed(Placed::Current),
            ));
        }
    }
    out
}

fn model_placement(
    inputs: &BTreeMap<Position, Content>,
    set_aside: &SetAside,
    weeks: &BTreeSet<i64>,
) -> BTreeMap<Position, Placement> {
    let held = inputs
        .iter()
        .map(|(&p, (item, _, commit_time))| {
            let h = match item {
                Item::Fact(r) => Held::Fact {
                    series: r.series_id,
                    t: r.t,
                    changed_at: r.changed_at,
                    set_aside: set_aside.holds(p),
                },
                Item::Entry(_) => Held::Entry {
                    commit_time: *commit_time,
                },
            };
            (p, h)
        })
        .collect();
    let last = inputs.values().map(|c| c.2).max().unwrap();
    place_l0(&held, last, weeks).unwrap()
}

/// Runs compaction over `segments`, written as files 1, 2, …, and checks every position is in exactly one output,
/// with its content, where the model places it.
fn check_lossless(segments: &[(u32, Vec<TestBatch>)], set_aside: &SetAside, weeks: &BTreeSet<i64>) {
    let dir = temp_dir("lossless");
    let cands: Vec<Candidate> = segments
        .iter()
        .enumerate()
        .map(|(i, (g, b))| {
            let f = i as u64 + 1;
            put_segment(&dir, *g, f, &segment_bytes(*g, f, b))
        })
        .collect();
    let outputs = run(&input(&dir, set_aside, weeks), &cands).unwrap();
    let inputs = input_contents(segments);
    let got = output_contents(&outputs);
    let expected = model_placement(&inputs, set_aside, weeks);
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        inputs.keys().collect::<Vec<_>>()
    );
    for (p, held) in &got {
        assert_eq!(held.len(), 1, "position {p:?} held {} times", held.len());
        assert_eq!(&held[0].0, &inputs[p], "content at {p:?}");
        assert_eq!(held[0].1, expected[p], "placement of {p:?}");
    }
    let ids: Vec<u64> = outputs.iter().map(|o| o.id.file_id).collect();
    assert_eq!(ids, (100..100 + outputs.len() as u64).collect::<Vec<_>>());
    for o in &outputs {
        assert!(o.path.to_string_lossy().ends_with(".kvc.tmp"));
        assert_eq!(o.layout.run_id, 100);
        assert_eq!(o.layout.run_count as usize, outputs.len());
        let named: Vec<u64> = o.layout.inputs.iter().map(|i| i.file_id).collect();
        assert_eq!(named, (1..=segments.len() as u64).collect::<Vec<_>>());
        assert!(o.layout.complete);
        assert_eq!(o.len, std::fs::metadata(&o.path).unwrap().len());
        let bytes = std::fs::read(&o.path).unwrap();
        assert_eq!(
            o.sha256,
            <[u8; 32]>::from(<sha2::Sha256 as sha2::Digest>::digest(&bytes))
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

fn value_for(series: u32, v: u16, absent: bool) -> Value {
    let vt = match series {
        1 => ValueType::F64,
        2 => ValueType::Text,
        3 => ValueType::Decimal,
        _ => ValueType::State,
    };
    if absent {
        return Value::Absent {
            value_type: vt,
            scale: if vt == ValueType::Decimal { 2 } else { 0 },
        };
    }
    match vt {
        ValueType::F64 => Value::F64(f64::from(v).to_bits()),
        ValueType::Text => Value::Text("x".repeat(usize::from(v % 300).min(255))),
        ValueType::Decimal => Value::Decimal {
            units: i64::from(v) - 100,
            scale: 2,
        },
        _ => Value::State(v),
    }
}

prop_compose! {
    fn item()(series in 1u32..=4, day in -40i64..=2, slot in 0i64..4, v in any::<u16>(), absent in any::<bool>(),
              late in any::<bool>(), quality in 0u8..4, changed in 0u64..3, entry in 0u8..8) -> Item {
        if entry == 0 {
            return Item::Entry(Box::new(Entry::Tzdata { version: format!("2026{}", char::from(b'a' + (v % 26) as u8)) }));
        }
        Item::Fact(FactRow {
            series_id: series,
            t: 10 * DAY + day * DAY + slot * 1_000,
            quality: match quality { 0 => Quality::Good, 1 => Quality::Estimated, 2 => Quality::Suspect, _ => Quality::Bad },
            flags: FactFlags { late, ..FactFlags::default() },
            value: value_for(series, v, absent),
            changed_at: (series == 4).then_some(changed),
        })
    }
}

prop_compose! {
    /// One to three segments of one to three batches; commit times rise within a generation; the last segment is
    /// sometimes of generation 1, whose sequences start again at 1.
    fn segments()(shape in prop::collection::vec(prop::collection::vec(
                      (prop::collection::vec(item(), 1..6), 0i64..3 * DAY, 0i64..1_000, 0u64..2), 1..4), 1..4),
                  second_generation in any::<bool>()) -> Vec<(u32, Vec<TestBatch>)> {
        let mut out = Vec::new();
        let mut seq = 1u64;
        let mut commit = 30 * DAY;
        let n = shape.len();
        for (i, batches) in shape.into_iter().enumerate() {
            let g = u32::from(second_generation && n > 1 && i + 1 == n);
            if g == 1 {
                seq = 1;
                commit -= DAY;
            }
            let mut bs = Vec::new();
            for (items, step, lag, gap) in batches {
                commit += step;
                seq += gap;
                let len: u64 = items.iter().map(Item::sequences).sum();
                bs.push(TestBatch { first: seq, received_at: commit - lag, commit_time: commit, items });
                seq += len;
            }
            out.push((g, bs));
        }
        out
    }
}

proptest! {
    #[test]
    fn every_input_position_lands_in_exactly_one_output_where_the_model_places_it(
        segs in segments(),
        merged in any::<bool>(),
    ) {
        let weeks = if merged { BTreeSet::from([3]) } else { BTreeSet::new() };
        check_lossless(&segs, &SetAside::new(vec![]), &weeks);
    }
}

fn restored_after_first() -> SetAside {
    SetAside::new(vec![RestoredEnd {
        generation: 1,
        end: (0, 1),
    }])
}

#[test]
fn set_aside_facts_go_to_set_aside_chunks_of_the_revision_file() {
    let segs = vec![(
        0,
        vec![
            batch(1, 30 * DAY, vec![reading(1, 29 * DAY, Value::F64(1))]),
            batch(2, 30 * DAY, vec![reading(1, 29 * DAY, Value::F64(2))]),
        ],
    )];
    check_lossless(&segs, &restored_after_first(), &BTreeSet::new());
}

#[test]
fn commit_times_may_fall_across_generations() {
    let segs = vec![
        (
            0,
            vec![batch(1, 30 * DAY, vec![reading(1, 5, Value::F64(1))])],
        ),
        (
            1,
            vec![batch(1, 29 * DAY, vec![reading(1, 6, Value::F64(1))])],
        ),
    ];
    check_lossless(&segs, &SetAside::new(vec![]), &BTreeSet::new());
}

/// Runs over `segs` expecting an error, and checks that nothing was written.
fn failure(segs: Vec<(u32, u64, Vec<u8>)>, set_aside: &SetAside) -> RunError {
    let dir = temp_dir("stall");
    let cands: Vec<Candidate> = segs
        .iter()
        .map(|(g, f, b)| put_segment(&dir, *g, *f, b))
        .collect();
    let weeks = BTreeSet::new();
    let e = run(&input(&dir, set_aside, &weeks), &cands).unwrap_err();
    assert!(!dir.join("data").exists(), "a stalled run writes nothing");
    std::fs::remove_dir_all(&dir).unwrap();
    e
}

fn cause(e: &RunError) -> Option<(Cause, u64)> {
    match e {
        RunError::Stalled(Stall { cause, file_id, .. }) => Some((*cause, *file_id)),
        _ => None,
    }
}

#[test]
fn a_set_aside_entry_stalls() {
    let segs = [
        batch(1, 30 * DAY, vec![reading(1, 29 * DAY, Value::F64(1))]),
        batch(
            2,
            30 * DAY,
            vec![Item::Entry(Box::new(Entry::Tzdata {
                version: "2026a".into(),
            }))],
        ),
    ];
    let e = failure(
        vec![(0, 1, segment_bytes(0, 1, &segs))],
        &restored_after_first(),
    );
    assert_eq!(cause(&e), Some((Cause::SetAsideEntry, 1)), "{e:?}");
}

#[test]
fn a_segment_with_a_block_of_unknown_type_stalls() {
    let mut img = image(0, 1, 1);
    let content = BatchContent {
        first_sequence: 1,
        received_at: 10,
        items: vec![reading(1, 5, Value::F64(1))],
    };
    let mut b = content.builder().unwrap();
    b.push_unknown(0x60, kvik_log::block::FLAG_SKIPPABLE, vec![1, 2, 3])
        .unwrap();
    img.append(&b, 10).unwrap();
    let bytes = seal(
        img,
        vec![IndexRecord {
            series_id: 1,
            fact_count: 1,
            min_t: 5,
            max_t: 5,
        }],
    );
    let e = failure(vec![(0, 1, bytes)], &SetAside::new(vec![]));
    assert_eq!(cause(&e), Some((Cause::Unknown, 1)), "{e:?}");
}

#[test]
fn commit_times_that_decrease_within_a_generation_stall_at_the_later_segment() {
    let a = segment_bytes(
        0,
        1,
        &[batch(1, 30 * DAY, vec![reading(1, 5, Value::F64(1))])],
    );
    let b = segment_bytes(
        0,
        2,
        &[batch(2, 29 * DAY, vec![reading(1, 6, Value::F64(1))])],
    );
    let e = failure(vec![(0, 1, a), (0, 2, b)], &SetAside::new(vec![]));
    assert_eq!(cause(&e), Some((Cause::CommitTimeDecreases, 2)), "{e:?}");
}

#[test]
fn a_damaged_segment_stalls_as_unreadable() {
    let mut a = segment_bytes(
        0,
        1,
        &[batch(1, 30 * DAY, vec![reading(1, 5, Value::F64(1))])],
    );
    a[200] ^= 0xFF;
    let e = failure(vec![(0, 1, a)], &SetAside::new(vec![]));
    assert!(
        matches!(cause(&e), Some((Cause::Unreadable(_), 1))),
        "{e:?}"
    );
}

#[test]
fn a_segment_whose_index_disagrees_with_its_facts_stalls() {
    let mut img = image(0, 1, 1);
    let content = BatchContent {
        first_sequence: 1,
        received_at: 10,
        items: vec![reading(1, 5, Value::F64(1)), reading(1, 6, Value::F64(1))],
    };
    img.append(&content.builder().unwrap(), 10).unwrap();
    let bytes = seal(
        img,
        vec![IndexRecord {
            series_id: 1,
            fact_count: 1,
            min_t: 5,
            max_t: 6,
        }],
    );
    let e = failure(vec![(0, 1, bytes)], &SetAside::new(vec![]));
    assert_eq!(
        cause(&e),
        Some((Cause::Unreadable("counts disagree with its seal"), 1)),
        "{e:?}"
    );
}

#[test]
fn a_time_whose_period_leaves_i64_stalls() {
    let a = segment_bytes(
        0,
        1,
        &[batch(
            1,
            30 * DAY,
            vec![reading(1, i64::MAX, Value::F64(1))],
        )],
    );
    let e = failure(vec![(0, 1, a)], &SetAside::new(vec![]));
    assert_eq!(cause(&e), Some((Cause::PeriodOutOfRange, 1)), "{e:?}");
}

#[test]
fn a_position_met_twice_with_the_same_content_is_kept_once() {
    let segs = [batch(
        1,
        30 * DAY,
        vec![reading(1, 5, Value::F64(1)), reading(2, 5, Value::F64(2))],
    )];
    let dir = temp_dir("twice");
    let a = put_segment(&dir, 0, 1, &segment_bytes(0, 1, &segs));
    let b = put_segment(&dir, 0, 2, &segment_bytes(0, 2, &segs));
    let set_aside = SetAside::new(vec![]);
    let weeks = BTreeSet::new();
    let got = output_contents(&run(&input(&dir, &set_aside, &weeks), &[a, b]).unwrap());
    assert_eq!(got.len(), 2);
    assert!(got.values().all(|v| v.len() == 1));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_position_met_twice_with_differing_content_abandons_the_run() {
    let a = segment_bytes(
        0,
        1,
        &[batch(1, 30 * DAY, vec![reading(1, 5, Value::F64(1))])],
    );
    let b = segment_bytes(
        0,
        2,
        &[batch(1, 30 * DAY, vec![reading(1, 5, Value::F64(2))])],
    );
    let e = failure(vec![(0, 1, a), (0, 2, b)], &SetAside::new(vec![]));
    assert!(matches!(e, RunError::Conflict((0, 1))), "{e:?}");
}

fn gauge(name: &str) -> Definition {
    Definition {
        id: 1,
        name: name.into(),
        kind: Kind::Gauge,
        value_type: ValueType::F64,
        scale: None,
        unit: String::new(),
        expected_interval: None,
        staleness: None,
        record_on_change: false,
        counter: None,
        lateness_bound: 0,
        state_names: Vec::new(),
        versioning: Versioning::None,
        timing: Timing::Observed,
        interval: None,
        retention: Retention::LongTerm,
        excess_precision: None,
        staleness_set: false,
    }
}

/// The series of an output in the order its chunks lie, which is name order (B.1).
fn series_order(o: &Output) -> Vec<u32> {
    let bytes = std::fs::read(&o.path).unwrap();
    let f = CompactedFile::open(bytes.as_slice(), &o.id, None).unwrap();
    let mut index = f.index().unwrap();
    index.sort_by_key(|e| e.chunk_offset);
    index.iter().map(|e| e.series_id).collect()
}

#[test]
fn a_definition_in_the_inputs_names_its_series_only_in_files_ending_after_it() {
    let both = |t: i64| vec![reading(1, t, Value::F64(1)), reading(2, t, Value::F64(2))];
    let mut renamed = gauge("a/a/new");
    renamed.id = 1;
    let segs = [
        batch(1, 30 * DAY, both(28 * DAY + 5)),
        batch(
            3,
            30 * DAY,
            vec![Item::Entry(Box::new(Entry::Definition(Box::new(renamed))))],
        ),
        batch(4, 30 * DAY, both(29 * DAY + 5)),
    ];
    let dir = temp_dir("names");
    let c = put_segment(&dir, 0, 1, &segment_bytes(0, 1, &segs));
    let set_aside = SetAside::new(vec![]);
    let weeks = BTreeSet::new();
    let registry = |id: u32| {
        let mut d = gauge(if id == 1 { "z/z/old" } else { "m/m/m" });
        d.id = id;
        Some(d)
    };
    let mut inp = input(&dir, &set_aside, &weeks);
    inp.registry = &registry;
    let outputs = run(&inp, &[c]).unwrap();
    let day = |start: i64| {
        outputs
            .iter()
            .find(|o| o.layout.period_start == start)
            .unwrap()
    };
    assert_eq!(series_order(day(28 * DAY)), vec![2, 1]);
    assert_eq!(series_order(day(29 * DAY)), vec![1, 2]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_interval_definition_gives_its_reach() {
    let mut d = gauge("a/b/c");
    d.kind = Kind::Interval;
    d.interval = Some(kvik_log::entries::IntervalLength::Fixed {
        length: 60,
        phase: 0,
    });
    assert_eq!(
        super::series_info(&d),
        (Some("a/b/c".into()), Reach::Fixed(60))
    );
    d.interval = Some(kvik_log::entries::IntervalLength::Month { zone: "UTC".into() });
    assert_eq!(super::series_info(&d).1, Reach::Month);
    d.interval = Some(kvik_log::entries::IntervalLength::Day {
        zone: "UTC".into(),
        day_start: 0,
    });
    assert_eq!(super::series_info(&d).1, Reach::Day);
    d.kind = Kind::Gauge;
    assert_eq!(super::series_info(&d).1, Reach::Point);
}

proptest! {
    #[test]
    fn the_period_of_a_time_is_the_models(
        day in -1_000i64..1_000,
        within in 0i64..DAY,
        delta in -2i64..=2,
        ahead in 0i64..20,
        merged in any::<bool>(),
    ) {
        let t = day * DAY + within;
        let last = (day + ahead) * DAY + delta;
        let weeks = if merged { BTreeSet::from([day.div_euclid(7)]) } else { BTreeSet::new() };
        prop_assert_eq!(super::period(t, last, &weeks), kvik_model::compaction::period(t, last, &weeks));
    }
}

#[test]
fn a_day_ending_exactly_fourteen_days_before_the_last_commit_keeps_its_day() {
    let none = BTreeSet::new();
    assert_eq!(super::period(5, 15 * DAY, &none), Some((0, DAY)));
    assert_eq!(super::period(5, 15 * DAY + 1, &none), Some((0, 7 * DAY)));
}

#[test]
fn the_period_of_a_time_at_the_edges_of_i64_is_the_models() {
    let none = BTreeSet::new();
    for t in [
        i64::MIN,
        i64::MIN + DAY,
        i64::MAX - 20 * DAY,
        i64::MAX - DAY,
        i64::MAX,
    ] {
        for last in [i64::MIN, 0, i64::MAX] {
            assert_eq!(
                super::period(t, last, &none),
                kvik_model::compaction::period(t, last, &none),
                "{t} {last}"
            );
        }
    }
}

/// The digest's share of a run (§3.2 step 2, Q-1410): run with `--release -- --ignored --nocapture`, once with
/// `RUSTFLAGS='--cfg sha2_backend="soft"'` for a core without SHA instructions.
#[test]
#[ignore = "a measurement"]
fn measure_the_digest_against_the_run() {
    let dir = temp_dir("digest");
    let (series, per_batch, batches) = (100u32, 100u64, 400u64);
    let t0 = 1_767_225_600_000_000_000i64;
    let mut segs = Vec::new();
    let mut first = 1u64;
    for file in 0..4u64 {
        let bs: Vec<TestBatch> = (0..batches / 4)
            .map(|b| {
                let k = file * batches / 4 + b;
                let items = (0..per_batch)
                    .map(|i| {
                        let n = k * per_batch + i;
                        let v = (n as f64 * 0.37).sin();
                        reading(
                            (n % u64::from(series)) as u32,
                            t0 + n as i64 * 10_000_000,
                            Value::F64(v.to_bits()),
                        )
                    })
                    .collect();
                let tb = batch(first, t0 + k as i64 * 1_000_000_000, items);
                first += per_batch;
                tb
            })
            .collect();
        segs.push(put_segment(
            &dir,
            1,
            1 + file,
            &segment_bytes(1, 1 + file, &bs),
        ));
    }
    let set_aside = SetAside::default();
    let weeks = BTreeSet::new();
    let facts = per_batch * batches;
    let t = std::time::Instant::now();
    let outputs = run(&input(&dir, &set_aside, &weeks), &segs).unwrap();
    let whole = t.elapsed();
    let sample = Fact::value(1, (1, 1), t0, 1, 0x4000_0000_0000_0000);
    let t = std::time::Instant::now();
    let mut acc = 0u128;
    for i in 0..facts * 2 {
        let mut f = sample.clone();
        f.position.1 = i;
        acc = acc.wrapping_add(super::fact_digest(&f, t0, t0));
    }
    let digest = t.elapsed();
    std::hint::black_box(acc);
    println!(
        "facts {facts}, outputs {}, run {:.1} ns/fact, digest (input and output) {:.1} ns/fact, share {:.1} %",
        outputs.len(),
        whole.as_nanos() as f64 / facts as f64,
        digest.as_nanos() as f64 / facts as f64,
        100.0 * digest.as_secs_f64() / whole.as_secs_f64()
    );
    std::fs::remove_dir_all(&dir).ok();
}
