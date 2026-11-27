//! What a rebuild does with compacted files no valid record names (compaction design §3.5): discarded only when
//! nothing names their run, every input is local and the run's files fail against them; otherwise live, or held.

mod support;

use kvik_files::desc::{FileClass, FileDescription, FileRef};
use kvik_files::layout::CompactedLayout;
use kvik_files::rebuild::{LocalFile, Place, RebuildInput, RebuildPlan, Verdict, Verifier, plan};
use kvik_files::record::{Body, Publish, Record};
use support::rebuild::image_with_corrupt;
use support::*;

/// Holds for a whole run whose files are not listed bad, fails a run with a bad file, cannot say for part of a run.
struct RunVerifier {
    bad: Vec<FileRef>,
}

impl Verifier for RunVerifier {
    fn verify(&self, outputs: &[&LocalFile], _: &[&LocalFile]) -> Verdict {
        let count = outputs[0]
            .layout
            .as_ref()
            .map_or(1, |l| l.run_count as usize);
        if outputs
            .iter()
            .any(|o| self.bad.contains(&o.description.file_ref()))
        {
            Verdict::Fails
        } else if outputs.len() < count {
            Verdict::CannotSay
        } else {
            Verdict::Holds
        }
    }
}

fn laid(d: &FileDescription, run_id: u64, run_count: u32) -> CompactedLayout {
    let mut l = layout_of(d);
    l.run_id = run_id;
    l.run_count = run_count;
    l
}

fn file(d: &FileDescription, l: Option<CompactedLayout>) -> LocalFile {
    let mut f = local(d, Place::Hot);
    f.layout = l;
    f
}

struct Case {
    segs: Vec<FileDescription>,
    a: FileDescription,
    b: FileDescription,
    run: Record,
}

/// Two segments; a run of two files over them, published by the record at index 1.
fn case() -> Case {
    let segs = vec![segment(1, 1, 10), segment(2, 11, 20)];
    let inputs = [seg_ref(1), seg_ref(2)];
    let a = compacted(100, FileClass::Current, (1, 10), &inputs);
    let b = compacted(101, FileClass::Current, (11, 20), &inputs);
    let run = Record::new(
        2,
        0,
        T0,
        Body::Publish(Publish {
            published: vec![a.clone(), b.clone()],
            retired: Vec::new(),
            layouts: vec![laid(&a, 7, 2), laid(&b, 7, 2)],
        }),
    );
    Case { segs, a, b, run }
}

fn rebuild(
    records: &[Record],
    corrupt: usize,
    files: &[LocalFile],
    v: &RunVerifier,
) -> RebuildPlan {
    let h = History {
        records: records.to_vec(),
        ..History::default()
    };
    let (_, img) = image_with_corrupt(&h, &[corrupt]);
    let input = RebuildInput {
        image: &img,
        files,
        losses: &[],
        lease_epoch: 0,
        mark_file_id: 0,
        written_at: T0,
    };
    plan(&input, v).unwrap()
}

fn live(p: &RebuildPlan, f: &FileDescription) -> bool {
    p.catalogue.file(f.file_ref()).is_some_and(|e| e.is_live())
}

#[test]
fn a_whole_run_that_verifies_is_published_again_with_its_layouts() {
    let c = case();
    let records = [
        Record::new(1, 0, T0, Body::Sealed(c.segs.clone())),
        c.run,
        Record::new(3, 0, T0, Body::Empty),
    ];
    let files: Vec<_> = c
        .segs
        .iter()
        .map(|s| file(s, None))
        .chain([
            file(&c.a, Some(laid(&c.a, 7, 2))),
            file(&c.b, Some(laid(&c.b, 7, 2))),
        ])
        .collect();
    let p = rebuild(&records, 1, &files, &RunVerifier { bad: Vec::new() });
    assert!(live(&p, &c.a) && live(&p, &c.b));
    assert_eq!(p.catalogue.layout(c.a.file_ref()), Some(&laid(&c.a, 7, 2)));
    assert!(p.discard.is_empty() && p.held.is_empty());
}

#[test]
fn a_run_that_fails_against_local_inputs_with_nothing_naming_it_is_discarded_whole() {
    let c = case();
    let records = [
        Record::new(1, 0, T0, Body::Sealed(c.segs.clone())),
        c.run,
        Record::new(3, 0, T0, Body::Empty),
    ];
    let files: Vec<_> = c
        .segs
        .iter()
        .map(|s| file(s, None))
        .chain([
            file(&c.a, Some(laid(&c.a, 7, 2))),
            file(&c.b, Some(laid(&c.b, 7, 2))),
        ])
        .collect();
    let p = rebuild(
        &records,
        1,
        &files,
        &RunVerifier {
            bad: vec![c.b.file_ref()],
        },
    );
    assert_eq!(p.discard, vec![c.a.file_ref(), c.b.file_ref()]);
    assert!(!live(&p, &c.a) && !live(&p, &c.b));
}

#[test]
fn a_run_record_corrupt_after_a_sibling_was_merged_away_and_removed_keeps_the_other_file() {
    let c = case();
    let m = compacted(102, FileClass::Current, (11, 20), &[c.b.file_ref()]);
    let merge = Record::new(
        3,
        0,
        T0,
        Body::Publish(Publish {
            published: vec![m.clone()],
            retired: vec![c.b.clone()],
            layouts: vec![laid(&m, 8, 1)],
        }),
    );
    let records = [
        Record::new(1, 0, T0, Body::Sealed(c.segs.clone())),
        c.run,
        merge,
        Record::new(4, 0, T0, Body::Empty),
    ];
    // The sibling is gone from disk; the merge's output and the inputs are local.
    let files: Vec<_> = c
        .segs
        .iter()
        .map(|s| file(s, None))
        .chain([
            file(&c.a, Some(laid(&c.a, 7, 2))),
            file(&m, Some(laid(&m, 8, 1))),
        ])
        .collect();
    for bad in [Vec::new(), vec![c.a.file_ref()]] {
        let p = rebuild(&records, 1, &files, &RunVerifier { bad });
        assert!(
            p.discard.is_empty(),
            "a file whose sibling a valid record names is never discarded"
        );
        assert_eq!(p.held, vec![c.a.file_ref()]);
        assert!(live(&p, &m));
    }
}

#[test]
fn a_run_record_corrupt_after_its_input_segments_were_released_keeps_its_files_live() {
    let c = case();
    let records = [
        Record::new(1, 0, T0, Body::Sealed(c.segs.clone())),
        c.run,
        Record::new(3, 0, T0, Body::Empty),
    ];
    // The segments left the local tiers; their sealed record is valid.
    let files = [
        file(&c.a, Some(laid(&c.a, 7, 2))),
        file(&c.b, Some(laid(&c.b, 7, 2))),
    ];
    let p = rebuild(
        &records,
        1,
        &files,
        &RunVerifier {
            bad: vec![c.a.file_ref()],
        },
    );
    assert!(live(&p, &c.a) && live(&p, &c.b));
    assert!(p.discard.is_empty() && p.held.is_empty());
}
