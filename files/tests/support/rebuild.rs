//! Helpers shared by the rebuild tests.

use std::collections::BTreeSet;

use kvik_files::appender::Gap;
use kvik_files::catalogue::Catalogue;
use kvik_files::desc::FileRef;
use kvik_files::image::{CorruptSpan, Image, read};
use kvik_files::rebuild::{LocalState, RebuildInput};
use kvik_files::record::{Body, Record};

use super::*;

pub const NOW: i64 = T0 + 1_000_000;

/// A history closed by an empty record, so that every record of the history has a successor.
pub fn closed(mut h: History) -> History {
    let e = h.records.last().unwrap().epoch + 1;
    h.records.push(Record::new(e, 0, T0, Body::Empty));
    h
}

pub fn image_with_corrupt(h: &History, indices: &[usize]) -> (Vec<u8>, Image) {
    let mut bytes = manifest_bytes(&h.records);
    for &i in indices {
        corrupt_record(&mut bytes, &h.records, i);
    }
    let img = read(&bytes, &STORE_ID).unwrap();
    (bytes, img)
}

pub fn input<'a>(img: &'a Image, h: &'a History) -> RebuildInput<'a> {
    RebuildInput {
        image: img,
        files: &h.files,
        losses: &h.losses,
        lease_epoch: 0,
        mark_file_id: h.mark_file_id,
        written_at: NOW,
    }
}

pub fn state(h: &History) -> LocalState<'_> {
    LocalState {
        files: &h.files,
        losses: &h.losses,
        lease_epoch: 0,
        mark_file_id: h.mark_file_id,
    }
}

pub fn live(c: &Catalogue) -> BTreeSet<FileRef> {
    c.live().map(|e| e.description.file_ref()).collect()
}

pub fn retired(c: &Catalogue) -> BTreeSet<FileRef> {
    c.files()
        .filter(|e| !e.is_live())
        .map(|e| e.description.file_ref())
        .collect()
}

/// A loss identity.
pub type Identity = (u32, u64);

/// The identities a corrupt record named as the loss's own (first) record or as an acknowledgement.
pub fn named_by(r: &Record) -> (Option<Identity>, Vec<Identity>) {
    match &r.body {
        Body::LostRange(l) if !l.extends => (Some(l.identity), Vec::new()),
        Body::Acknowledgement(a) => (None, a.losses.iter().map(|l| l.identity).collect()),
        _ => (None, Vec::new()),
    }
}

/// Segments 1 and 2, compacted into 1 (current) and 2 (revision); segment 3 compacted into 3; 1 and 3 merged
/// into 4 (retiring them); then an empty record.
pub fn store() -> History {
    closed(play(&[
        Step::Seal {
            len: 6,
            lost: None,
            cause_given: false,
        },
        Step::Seal {
            len: 6,
            lost: None,
            cause_given: false,
        },
        Step::Compact { two: true },
        Step::Seal {
            len: 6,
            lost: None,
            cause_given: false,
        },
        Step::Compact { two: false },
        Step::Merge,
    ]))
}

pub fn index_of(h: &History, pred: impl Fn(&Body) -> bool) -> usize {
    h.records.iter().position(|r| pred(&r.body)).unwrap()
}

pub fn merge_index(h: &History) -> usize {
    index_of(
        h,
        |b| matches!(b, Body::Publish(p) if !p.retired.is_empty()),
    )
}

pub fn lost_record_history() -> (History, usize) {
    let h = closed(play(&[
        Step::Seal {
            len: 6,
            lost: Some(2),
            cause_given: false,
        },
        Step::Seal {
            len: 6,
            lost: None,
            cause_given: false,
        },
        Step::Compact { two: false },
    ]));
    let i = index_of(&h, |b| matches!(b, Body::LostRange(_)));
    (h, i)
}

/// The gap of a corrupt span.
pub fn gap(s: &CorruptSpan) -> Gap {
    Gap {
        after: s.after_epoch,
        before: s.before_epoch,
    }
}
