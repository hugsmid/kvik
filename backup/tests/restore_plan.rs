//! The restore plan (`spec.md` §8 Restore step 1): scenarios for each rule and error, Q-108's record selection, and
//! properties over generated histories. A copy-verified record (`0x27`) is laid out by roadmap 8.1c; here a
//! placeholder record of that type stands for one, and its fold is [`Catalogue::record_copy`].

use std::collections::BTreeSet;

use kvik_backup::plan::{PlanError, PlanInput, PlanMode, RestorePlan, plan, records_to_read};
use kvik_files::catalogue::{Catalogue, Tier};
use kvik_files::desc::{FileClass, FileDescription, FileRef, kind};
use kvik_files::record::{AckedLoss, Acknowledgement, Body, Rebuild, Record, record_type};
use kvik_files::restore::{RestoreRecord, RestoredFile};
use kvik_files::tier::DeepCopy;
use kvik_log::manifest::{LossCause, LostRange, RecordPosition};
use kvik_log::mark::ResetStamp;
use kvik_log::setaside::RestoredEnd;
use proptest::prelude::*;

fn seg(generation: u32, file_id: u64, first: u64, last: u64) -> FileDescription {
    FileDescription {
        kind: kind::LOG_SEGMENT,
        class: FileClass::None,
        generation,
        file_id,
        range: Some(((generation, first), (generation, last))),
        hash: [file_id as u8; 32],
        inputs: Vec::new(),
    }
}

fn seg_ref(file_id: u64) -> FileRef {
    FileRef {
        kind: kind::LOG_SEGMENT,
        file_id,
    }
}

fn copy_of(file_id: u64) -> DeepCopy {
    DeepCopy {
        size: 1000 + file_id,
        sha256: [(file_id as u8).wrapping_add(1); 32],
    }
}

fn lost(first: RecordPosition, last: RecordPosition) -> LostRange {
    LostRange {
        identity: first,
        first,
        last,
        file_id: 0,
        offset: 0,
        length: 0,
        cause: LossCause::TruncatedTail,
        boot_id: [2; 16],
        id_floor: 0,
        time_before: None,
        time_after: None,
        extends: false,
        stamp: None,
    }
}

/// Records of one generation; epochs are given in order by [`History::records`].
#[derive(Clone, Debug, Default)]
struct History {
    generation: u32,
    bodies: Vec<(Body, Option<u64>)>,
}

impl History {
    fn new(generation: u32) -> Self {
        Self {
            generation,
            bodies: Vec::new(),
        }
    }
    fn push(&mut self, b: Body) -> &mut Self {
        self.bodies.push((b, None));
        self
    }
    fn seal(&mut self, d: FileDescription) -> &mut Self {
        self.push(Body::Sealed(vec![d]))
    }
    fn copy(&mut self, file_id: u64) -> &mut Self {
        self.push(copy_body(file_id))
    }
    fn lose(&mut self, first: RecordPosition, last: RecordPosition) -> &mut Self {
        self.push(Body::LostRange(lost(first, last)))
    }
    fn records(&self) -> Vec<Record> {
        self.bodies
            .iter()
            .enumerate()
            .map(|(i, (b, old))| Record {
                old_epoch: *old,
                ..Record::new(i as u64 + 1, self.generation, 0, b.clone())
            })
            .collect()
    }
}

fn copy_body(file_id: u64) -> Body {
    Body::Other {
        record_type: record_type::TIER_MOVE,
        skippable: false,
        fields: vec![(1, file_id.to_le_bytes().to_vec())],
    }
}

/// The fold of `records`, the placeholder copy records folded by [`Catalogue::record_copy`]. A carried one (field
/// 100) marks no copy verified (Q-280), so it is not folded.
fn catalogue(records: &[Record]) -> Catalogue {
    let mut c = Catalogue::new();
    for r in records {
        c.apply(r).unwrap();
        if let Body::Other {
            record_type: record_type::TIER_MOVE,
            fields,
            ..
        } = &r.body
            && r.old_epoch.is_none()
        {
            let id = u64::from_le_bytes(fields[0].1[..].try_into().unwrap());
            c.record_copy(seg_ref(id), Tier::Deep, copy_of(id));
        }
    }
    c
}

fn input<'a>(c: &'a Catalogue, after: &'a [RecordPosition]) -> PlanInput<'a> {
    PlanInput {
        generation: c.generation() + 1,
        nonce: 77,
        source_generation: c.generation(),
        source: c,
        mode: PlanMode::Full,
        recovery_after: after,
    }
}

fn plan_of(h: &History) -> RestorePlan {
    plan(&input(&catalogue(&h.records()), &[])).unwrap()
}

fn listed(p: &RestorePlan) -> Vec<u64> {
    p.record
        .files
        .iter()
        .map(|f| f.description.file_id)
        .collect()
}

fn gen0() -> History {
    let mut h = History::new(0);
    h.seal(seg(0, 1, 1, 10))
        .copy(1)
        .seal(seg(0, 2, 11, 20))
        .copy(2);
    h
}

#[test]
fn the_restored_end_is_the_last_copied_position_and_the_record_lists_each_copy() {
    let p = plan_of(&gen0());
    assert_eq!(p.record.restored_end, (0, 20));
    assert_eq!(p.record.generation, 1);
    assert_eq!(
        p.record.stamp,
        ResetStamp {
            sequence: 1,
            nonce: 77
        }
    );
    assert_eq!(listed(&p), vec![1, 2]);
    assert_eq!(p.record.files[1].copy, copy_of(2));
    assert_eq!(p.record.files[1].tier, Tier::Hot);
    assert_eq!(p.fetch, p.record.files);
    assert!(p.record.carried_ends.is_empty());
    assert!(!p.record.drill);
}

#[test]
fn a_listed_segment_with_no_copy_ends_the_restored_log_and_is_not_named() {
    let mut h = gen0();
    h.seal(seg(0, 3, 21, 30)).seal(seg(0, 4, 31, 40)).copy(4);
    let p = plan_of(&h);
    assert_eq!(p.record.restored_end, (0, 20));
    assert_eq!(listed(&p), vec![1, 2, 4]);
    // The copied segment past the end is set aside, listed and fetched.
    assert!(p.record.set_aside().holds((0, 31)));
    assert_eq!(p.fetch.last().unwrap().description.file_id, 4);
}

#[test]
fn a_gap_a_recorded_loss_covers_does_not_end_the_restored_log_a_partly_covered_one_does() {
    let mut h = gen0();
    h.lose((0, 21), (0, 24)).seal(seg(0, 3, 25, 30)).copy(3);
    assert_eq!(plan_of(&h).record.restored_end, (0, 30));
    let mut h = gen0();
    h.lose((0, 21), (0, 23)).seal(seg(0, 3, 25, 30)).copy(3);
    assert_eq!(plan_of(&h).record.restored_end, (0, 20));
    let mut h = gen0();
    h.lose((0, 22), (0, 24)).seal(seg(0, 3, 25, 30)).copy(3);
    assert_eq!(plan_of(&h).record.restored_end, (0, 20));
}

#[test]
fn a_tail_loss_leaves_the_restored_end_at_the_last_held_position() {
    let mut h = gen0();
    h.lose((0, 21), (0, 24));
    assert_eq!(plan_of(&h).record.restored_end, (0, 20));
}

#[test]
fn a_gap_followed_by_a_recovery_entry_naming_the_position_before_it_does_not_end_the_restored_log()
{
    let mut h = gen0();
    h.seal(seg(0, 3, 25, 30)).copy(3);
    let c = catalogue(&h.records());
    let end = |after: &[RecordPosition]| plan(&input(&c, after)).unwrap().record.restored_end;
    assert_eq!(end(&[]), (0, 20));
    assert_eq!(end(&[(0, 20)]), (0, 30));
    assert_eq!(end(&[(0, 19)]), (0, 20));
    assert_eq!(end(&[(1, 20)]), (0, 20));
    // A loss over the start of the gap: the entry names the loss's end as `after`.
    let mut h = gen0();
    h.lose((0, 21), (0, 22)).seal(seg(0, 3, 25, 30)).copy(3);
    let c = catalogue(&h.records());
    assert_eq!(
        plan(&input(&c, &[(0, 22)])).unwrap().record.restored_end,
        (0, 30)
    );
    // A loss after an uncovered part: no entry follows that part directly.
    let mut h = gen0();
    h.lose((0, 23), (0, 24)).seal(seg(0, 3, 25, 30)).copy(3);
    let c = catalogue(&h.records());
    assert_eq!(
        plan(&input(&c, &[(0, 20)])).unwrap().record.restored_end,
        (0, 20)
    );
}

#[test]
fn a_recovery_entry_in_a_segment_with_no_copy_bridges_nothing() {
    let mut h = gen0();
    h.seal(seg(0, 3, 25, 30)).seal(seg(0, 4, 31, 40)).copy(4);
    let c = catalogue(&h.records());
    let p = plan(&input(&c, &[(0, 20)])).unwrap();
    assert_eq!(p.record.restored_end, (0, 20));
}

#[test]
fn a_copy_no_longer_marked_verified_in_deep_is_not_restored_from() {
    let mut h = gen0();
    h.push(Body::Rebuild(Rebuild::default()));
    let c = catalogue(&h.records());
    assert!(c.file(seg_ref(1)).unwrap().deep.is_some());
    let p = plan(&input(&c, &[])).unwrap();
    assert_eq!(p.record.restored_end, (0, 0));
    assert!(p.record.files.is_empty());
}

#[test]
fn a_store_whose_first_position_is_not_copied_restores_to_the_origin() {
    let mut h = History::new(0);
    h.seal(seg(0, 1, 1, 10)).seal(seg(0, 2, 11, 20)).copy(2);
    let p = plan_of(&h);
    assert_eq!(p.record.restored_end, (0, 0));
    assert_eq!(listed(&p), vec![2]);
}

fn read_record(end: RecordPosition, files: &[(FileDescription, u64)]) -> RestoreRecord {
    RestoreRecord {
        generation: 1,
        stamp: ResetStamp {
            sequence: 1,
            nonce: 5,
        },
        source_generation: 0,
        restored_end: end,
        carried_ends: Vec::new(),
        losses: vec![lost((0, 3), (0, 3))],
        acknowledgements: vec![AckedLoss {
            identity: (0, 3),
            first: (0, 3),
            last: (0, 3),
        }],
        entry_losses: vec![lost((0, 30), (0, 30))],
        files: files
            .iter()
            .map(|(d, id)| RestoredFile {
                description: d.clone(),
                tier: Tier::Hot,
                copy: copy_of(*id),
            })
            .collect(),
        drill: false,
    }
}

/// Generation 1, restored to `0:20` from segments 1 and 2 with segment 3 (`0:21` to `0:30`) set aside.
fn gen1() -> History {
    let mut h = History::new(1);
    h.push(
        read_record(
            (0, 20),
            &[
                (seg(0, 1, 1, 10), 1),
                (seg(0, 2, 11, 20), 2),
                (seg(0, 3, 21, 30), 3),
            ],
        )
        .to_body(),
    );
    h
}

#[test]
fn a_generation_with_no_position_restores_to_the_end_of_the_record_it_read() {
    let p = plan_of(&gen1());
    assert_eq!(p.record.generation, 2);
    assert_eq!(p.record.source_generation, 1);
    assert_eq!(p.record.restored_end, (0, 20));
    assert_eq!(
        p.record.carried_ends,
        vec![RestoredEnd {
            generation: 1,
            end: (0, 20)
        }]
    );
    assert_eq!(listed(&p), vec![1, 2, 3]);
}

#[test]
fn the_source_generation_is_scanned_from_sequence_1_and_carries_the_ends_losses_and_acknowledgements()
 {
    let mut h = gen1();
    h.seal(seg(1, 4, 1, 8))
        .copy(4)
        .seal(seg(1, 5, 9, 12))
        .lose((1, 13), (1, 13))
        .push(Body::Acknowledgement(Acknowledgement {
            losses: vec![AckedLoss {
                identity: (1, 13),
                first: (1, 13),
                last: (1, 13),
            }],
            rebuild: None,
        }));
    let p = plan_of(&h);
    assert_eq!(p.record.restored_end, (1, 8));
    assert_eq!(listed(&p), vec![1, 2, 3, 4]);
    let s = p.record.set_aside();
    assert!(s.holds((0, 21)) && s.holds((1, 9)) && !s.holds((1, 8)) && !s.holds((0, 20)));
    let identities: Vec<_> = p.record.losses.iter().map(|l| l.identity).collect();
    assert_eq!(identities, vec![(0, 3), (1, 13)]);
    assert_eq!(p.record.acknowledgements.len(), 2);
    assert_eq!(p.record.entry_losses, vec![lost((0, 30), (0, 30))]);
}

#[test]
fn a_restore_of_a_restore_carries_the_read_records_own_end_then_its_carried_ones() {
    let mut first = gen1();
    first.seal(seg(1, 4, 1, 8)).copy(4);
    let p1 = plan_of(&first);
    let mut second = History::new(2);
    second.push(p1.record.to_body());
    let p2 = plan_of(&second);
    assert_eq!(p2.record.restored_end, (1, 8));
    assert_eq!(
        p2.record.carried_ends,
        vec![
            RestoredEnd {
                generation: 2,
                end: (1, 8)
            },
            RestoredEnd {
                generation: 1,
                end: (0, 20)
            }
        ]
    );
    assert_eq!(listed(&p2), listed(&p1));
}

#[test]
fn a_file_missing_at_or_below_a_carried_end_fails_the_plan() {
    let mut h = History::new(1);
    h.push(read_record((0, 20), &[(seg(0, 1, 1, 10), 1)]).to_body());
    let c = catalogue(&h.records());
    assert_eq!(
        plan(&input(&c, &[])),
        Err(PlanError::MissingBelowEnd { at: (0, 11) })
    );
    h.seal(seg(0, 2, 11, 20));
    let c = catalogue(&h.records());
    assert_eq!(
        plan(&input(&c, &[])),
        Err(PlanError::NotCopied { file: seg_ref(2) })
    );
    // A recorded loss there is no missing file.
    let mut h = History::new(1);
    h.push(read_record((0, 20), &[(seg(0, 1, 1, 10), 1)]).to_body())
        .lose((0, 11), (0, 20));
    assert_eq!(plan_of(&h).record.restored_end, (0, 20));
}

#[test]
fn the_plan_refuses_what_it_cannot_plan() {
    let empty = Catalogue::new();
    assert_eq!(plan(&input(&empty, &[])), Err(PlanError::NoRecords));
    let c = catalogue(&gen0().records());
    let at = |f: fn(&mut PlanInput<'_>)| {
        let mut i = input(&c, &[]);
        f(&mut i);
        plan(&i)
    };
    assert!(matches!(
        at(|i| i.generation = 0),
        Err(PlanError::Invalid(_))
    ));
    assert!(matches!(at(|i| i.nonce = 0), Err(PlanError::Invalid(_))));
    assert!(matches!(
        at(|i| i.source_generation = 1),
        Err(PlanError::Invalid(_))
    ));
    assert!(matches!(
        at(|i| i.source_generation = u32::MAX),
        Err(PlanError::GenerationsExhausted)
    ));
    let mut h = History::new(1);
    h.seal(seg(1, 1, 1, 5)).copy(1);
    assert!(matches!(
        plan(&input(&catalogue(&h.records()), &[])),
        Err(PlanError::Invalid(_))
    ));
}

#[test]
fn a_drill_marks_its_record_and_plans_what_a_restore_would() {
    let c = catalogue(&gen0().records());
    let full = plan(&input(&c, &[])).unwrap();
    let drill = plan(&PlanInput {
        mode: PlanMode::Drill,
        ..input(&c, &[])
    })
    .unwrap();
    assert!(drill.record.drill);
    assert_eq!(
        RestoreRecord {
            drill: false,
            ..drill.record
        },
        full.record
    );
}

#[test]
fn a_compacted_file_on_hot_is_listed_and_fetched_but_never_fills_a_gap() {
    let mut h = History::new(0);
    let comp = FileDescription {
        kind: kind::COMPACTED,
        class: FileClass::Current,
        generation: 0,
        file_id: 1,
        range: Some(((0, 1), (0, 20))),
        hash: [8; 32],
        inputs: vec![seg_ref(1)],
    };
    h.seal(seg(0, 1, 1, 10))
        .copy(1)
        .push(Body::Publish(kvik_files::record::Publish {
            published: vec![comp.clone()],
            retired: Vec::new(),
        }));
    let mut c = catalogue(&h.records());
    c.record_copy(comp.file_ref(), Tier::Deep, copy_of(9));
    let p = plan(&input(&c, &[])).unwrap();
    assert_eq!(p.record.restored_end, (0, 10));
    assert_eq!(p.record.files.len(), 2);
    assert_eq!(p.fetch.len(), 2);
}

// ---- Q-108: which records a restore reads ----

/// Old records, then a rebuilt manifest that carries them, cut after `upload` of its records.
fn rebuilt(old: &History, upload: usize) -> Vec<Record> {
    let mut h = old.clone();
    let olds = old.records();
    h.push(Body::Rebuild(Rebuild::default()));
    for r in &olds {
        h.bodies.push((r.body.clone(), Some(r.epoch)));
    }
    h.push(Body::Empty);
    let all = h.records();
    all[..olds.len() + upload].to_vec()
}

#[test]
fn q108_a_rebuilt_manifest_only_partly_uploaded_is_not_read() {
    let mut old = gen0();
    old.seal(seg(0, 3, 21, 30)).copy(3);
    let n = old.records().len();
    for upload in 0..=n + 1 {
        let records = rebuilt(&old, upload);
        let read = records_to_read(&records);
        assert_eq!(read, &records[..n], "upload {upload}");
        assert_eq!(
            plan(&input(&catalogue(read), &[]))
                .unwrap()
                .record
                .restored_end,
            (0, 30)
        );
    }
    // Whole, it is read; its files count once verified in deep again after the rebuild (Q-280, Q-294).
    let whole = rebuilt(&old, n + 2);
    let read = records_to_read(&whole);
    assert_eq!(read, &whole[n..]);
    let end = |r: &[Record]| {
        plan(&input(&catalogue(r), &[]))
            .unwrap()
            .record
            .restored_end
    };
    assert_eq!(end(read), (0, 0));
    let mut again = History::new(0);
    again.bodies = whole
        .iter()
        .map(|r| (r.body.clone(), r.old_epoch))
        .collect();
    again.copy(1).copy(2).copy(3);
    let records = again.records();
    assert_eq!(end(records_to_read(&records)), (0, 30));
    // Reading from the rebuild record as soon as it is uploaded would read a truncated list of files.
    let partial = rebuilt(&old, 3);
    assert_eq!(catalogue(&partial[n..]).files().count(), 1);
    assert_eq!(catalogue(records_to_read(&partial)).files().count(), 3);
}

#[test]
fn q108_records_after_a_whole_rebuilt_manifest_are_read_with_it_and_a_later_partial_one_is_not() {
    let old = gen0();
    let n = old.records().len();
    let mut records = rebuilt(&old, n + 2);
    let mut later = History::new(0);
    later.bodies = records
        .iter()
        .map(|r| (r.body.clone(), r.old_epoch))
        .collect();
    later.copy(1).copy(2).seal(seg(0, 3, 21, 30)).copy(3);
    records = later.records();
    let read = records_to_read(&records);
    assert_eq!(read, &records[n..]);
    assert_eq!(
        plan(&input(&catalogue(read), &[]))
            .unwrap()
            .record
            .restored_end,
        (0, 30)
    );
    // A second rebuild, its carried records partly uploaded.
    later.push(Body::Rebuild(Rebuild::default()));
    later
        .bodies
        .push((records[n].body.clone(), Some(records[n].epoch)));
    let records = later.records();
    let read = records_to_read(&records);
    assert_eq!(read.len(), records.len() - n - 2);
    assert_eq!(read[0], records[n]);
}

// ---- Properties ----

#[derive(Clone, Copy, Debug)]
enum Gap {
    None,
    Lost,
    Recovery,
    Plain,
}

#[derive(Clone, Debug)]
struct Gen {
    history: History,
    after: Vec<RecordPosition>,
    /// For each segment: id, first and last sequence, copied.
    segments: Vec<(u64, u64, u64, bool)>,
    losses: Vec<(u64, u64)>,
}

fn gap() -> impl Strategy<Value = Gap> {
    prop_oneof![
        6 => Just(Gap::None),
        1 => Just(Gap::Lost),
        1 => Just(Gap::Recovery),
        1 => Just(Gap::Plain),
    ]
}

/// A generation-0 history: segments with gaps between them, copies made late and in any order, and, if `rebuild`
/// holds, a whole rebuilt manifest after some record.
fn history(rebuild: bool) -> impl Strategy<Value = Gen> {
    (
        proptest::collection::vec((gap(), 1u64..4, 1u64..6, prop::bool::weighted(0.8)), 1..10),
        proptest::collection::vec(any::<prop::sample::Index>(), 10),
        proptest::option::of(any::<prop::sample::Index>()).prop_map(move |r| r.filter(|_| rebuild)),
    )
        .prop_map(|(segs, places, rebuild)| {
            let mut h = History::new(0);
            h.push(Body::Empty);
            let mut after = Vec::new();
            let mut segments = Vec::new();
            let mut losses = Vec::new();
            let mut next = 1u64;
            for (i, (g, gap_len, len, copied)) in segs.into_iter().enumerate() {
                let id = i as u64 + 1;
                if i > 0 || !matches!(g, Gap::Recovery) {
                    match g {
                        Gap::None => {}
                        Gap::Lost => {
                            h.lose((0, next), (0, next + gap_len - 1));
                            losses.push((next, next + gap_len - 1));
                            next += gap_len;
                        }
                        Gap::Recovery => {
                            after.push((0, next - 1));
                            next += gap_len;
                        }
                        Gap::Plain => next += gap_len,
                    }
                }
                h.seal(seg(0, id, next, next + len - 1));
                segments.push((id, next, next + len - 1, copied));
                next += len;
            }
            for (k, (id, _, _, copied)) in segments.iter().enumerate() {
                if *copied {
                    let seal_at = h
                        .bodies
                        .iter()
                        .position(|(b, _)| matches!(b, Body::Sealed(d) if d[0].file_id == *id))
                        .unwrap();
                    let span = h.bodies.len() - seal_at;
                    let at = seal_at + 1 + places[k % places.len()].index(span);
                    h.bodies.insert(at, (copy_body(*id), None));
                }
            }
            if let Some(at) = rebuild {
                let cut = 1 + at.index(h.bodies.len());
                let carried: Vec<(Body, Option<u64>)> = h.bodies[..cut]
                    .iter()
                    .enumerate()
                    .map(|(i, (b, _))| (b.clone(), Some(i as u64 + 1)))
                    .collect();
                let mut bodies = h.bodies[..cut].to_vec();
                bodies.push((Body::Rebuild(Rebuild::default()), None));
                bodies.extend(carried);
                bodies.push((Body::Empty, None));
                bodies.extend(h.bodies[cut..].iter().cloned());
                h.bodies = bodies;
            }
            Gen {
                history: h,
                after,
                segments,
                losses,
            }
        })
}

/// The restored end of `spec.md` §8 Restore step 1, walked one position at a time.
fn walked_end(g: &Gen) -> RecordPosition {
    let listed = |q: u64| g.segments.iter().any(|s| s.1 <= q && q <= s.2);
    let held = |q: u64| g.segments.iter().any(|s| s.3 && s.1 <= q && q <= s.2);
    let lost = |q: u64| g.losses.iter().any(|l| l.0 <= q && q <= l.1);
    let top = g.segments.iter().map(|s| s.2).max().unwrap_or(0);
    let mut end = 0;
    let mut q = 1;
    while q <= top {
        if held(q) {
            end = q;
        } else if !lost(q) {
            let Some(next) = (q..=top).find(|x| listed(*x)) else {
                break;
            };
            let bridged =
                held(next) && (q..next).all(|x| !lost(x)) && g.after.contains(&(0, q - 1));
            if !bridged {
                break;
            }
            q = next;
            continue;
        }
        q += 1;
    }
    (0, end)
}

proptest! {
    #[test]
    fn the_restored_end_is_the_one_the_spec_walks_to(g in history(false)) {
        let records = g.history.records();
        let c = catalogue(records_to_read(&records));
        let p = plan(&input(&c, &g.after)).unwrap();
        prop_assert_eq!(p.record.restored_end, walked_end(&g));
    }

    #[test]
    fn the_plan_names_every_copied_file_and_never_one_without_a_copy(g in history(true)) {
        let records = g.history.records();
        let c = catalogue(records_to_read(&records));
        let p = plan(&input(&c, &g.after)).unwrap();
        for f in &p.record.files {
            prop_assert_eq!(c.file(f.description.file_ref()).unwrap().deep.as_ref(), Some(&f.copy));
        }
        let named: BTreeSet<FileRef> = p.record.files.iter().map(|f| f.description.file_ref()).collect();
        let copied: BTreeSet<FileRef> =
            c.live().filter(|e| e.deep.is_some() && e.verified.contains(&Tier::Deep)).map(|e| e.description.file_ref()).collect();
        prop_assert_eq!(&named, &copied);
        prop_assert!(p.fetch.iter().all(|f| named.contains(&f.description.file_ref())));
        prop_assert_eq!(RestoreRecord::from_body(&p.record.to_body()), Some(Ok(p.record.clone())));
    }

    #[test]
    fn the_plan_is_deterministic_and_independent_of_the_order_copies_arrive_in(g in history(true)) {
        let records = g.history.records();
        let read = records_to_read(&records);
        let a = plan(&input(&catalogue(read), &g.after));
        prop_assert_eq!(&a, &plan(&input(&catalogue(read), &g.after)));
        let (copies, rest): (Vec<Record>, Vec<Record>) = read
            .iter()
            .cloned()
            .partition(|r| matches!(r.body, Body::Other { .. }));
        let mut moved: Vec<Record> = rest.into_iter().chain(copies.into_iter().rev()).collect();
        for (i, r) in moved.iter_mut().enumerate() {
            r.epoch = i as u64 + 1;
        }
        prop_assert_eq!(a, plan(&input(&catalogue(&moved), &g.after)));
    }

    #[test]
    fn the_plan_is_monotone_in_the_records_uploaded(g in history(false), cut in any::<prop::sample::Index>()) {
        let records = g.history.records();
        let k = cut.index(records.len()) + 1;
        let full = catalogue(records_to_read(&records));
        let part = catalogue(records_to_read(&records[..k]));
        let a = plan(&input(&part, &g.after)).unwrap();
        let b = plan(&input(&full, &g.after)).unwrap();
        prop_assert!(a.record.restored_end <= b.record.restored_end);
        let named = |p: &RestorePlan| -> BTreeSet<FileRef> {
            p.record.files.iter().map(|f| f.description.file_ref()).collect()
        };
        prop_assert!(named(&a).is_subset(&named(&b)));
    }
}
