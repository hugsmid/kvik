//! A corrupt span with no later record (Q-650): kept, its rebuild and copies, and the epochs a rebuild takes above
//! it (Q-872).

mod support;

use kvik_files::appender::{Appender, Gap, Opened, RebuildState};
use kvik_files::image::{CorruptSpan, read};
use kvik_files::rebuild::{Answer, Finality, plan, rebuild_manifest, repair_with_copy, resolve};
use kvik_files::record::{Body, Record};
use support::rebuild::*;
use support::*;

#[test]
fn q650_a_crafted_tail_with_no_later_record_is_kept_corrupt_and_its_rebuild_names_an_open_span() {
    let h = store();
    let mut bytes = manifest_bytes(&h.records);
    let last = h.records.last().unwrap().epoch;
    let valid_end = bytes.len() as u64;
    crafted_tail(&mut bytes, last, 2_000, 100_000);
    let img = read(&bytes, &STORE_ID).unwrap();
    assert_eq!(img.records.len(), h.records.len());
    assert!(!img.torn);
    assert_eq!(
        img.corrupt,
        [CorruptSpan {
            offset: valid_end,
            end: bytes.len() as u64,
            after_epoch: last,
            before_epoch: None,
            named_epoch: last + 2_000,
        }]
    );
    let fs = fs_with(&bytes);
    assert!(matches!(
        Appender::open(&fs, &dir(), STORE_ID, 0).unwrap(),
        Opened::Corrupt(_)
    ));
    assert_eq!(
        fs.read(&dir().join("MANIFEST")).unwrap(),
        bytes,
        "kept, not truncated as a torn tail"
    );
    let (app, p) = rebuild_manifest(
        &fs,
        &dir(),
        STORE_ID,
        state(&h),
        &FakeVerifier::default(),
        Finality::Provisional,
        NOW,
    )
    .unwrap();
    let rb = p.rebuild_record().unwrap();
    assert_eq!(rb.gaps, [(last, None)]);
    // The crafted images name epochs up to `last` + 2,000; the span's length leaves room for more (Q-872).
    let room = (bytes.len() as u64 - valid_end) / 40;
    assert!(room > 2_000);
    assert_eq!(p.rebuild_epoch, last + room + 1);
    assert_eq!(rb.presumed_epoch, last + 1);
    let g = Gap {
        after: last,
        before: None,
    };
    assert_eq!(app.pending(), &[g]);
    // After a restart the open span comes back from field 7.
    drop(app);
    let Opened::Ready(mut app) = Appender::open(&fs, &dir(), STORE_ID, 0).unwrap() else {
        panic!("the rebuilt manifest reads")
    };
    assert_eq!(app.pending(), &[g]);
    // A copy of the corrupt record is any valid record above the last valid epoch.
    let below = Record::new(last, 0, T0, Body::Empty).encode().unwrap();
    assert!(resolve(&mut app, g, &Answer::HoldsNone, &Answer::Holds(below)).is_err());
    let copy = Record::new(last + 1, 0, T0, Body::Empty).encode().unwrap();
    assert_eq!(
        resolve(&mut app, g, &Answer::HoldsNone, &Answer::Holds(copy)).unwrap(),
        RebuildState::Final {
            epoch: p.rebuild_epoch
        }
    );
}

/// Appends a damaged word, then `m` lost-range records sharing one list of `g` ignorable empty fields, each with a
/// valid checksum, an epoch above `min` and no field 1: failed reads that use a window's budget (Q-650).
fn crafted_tail(f: &mut Vec<u8>, min: u64, m: u64, g: u64) {
    use kvik_log::block::BlockHeader;
    use kvik_log::crc::{crc32c, crc32c_shift};
    let put = |f: &mut Vec<u8>, at: u64, b: &[u8]| {
        f[at as usize..at as usize + b.len()].copy_from_slice(b);
    };
    let field = 0x8001u16;
    f.extend_from_slice(&[0xFF; 8]);
    let base = f.len() as u64;
    let e = (base + 48 * m + 4 * g).div_ceil(8) * 8;
    f.resize(e as usize, 0);
    let shared = base + 48 * m - 8;
    for i in 0..(e - shared) / 4 {
        put(f, shared + 4 * i, &field.to_le_bytes());
    }
    let mut inner: Option<u32> = None;
    for k in (0..m).rev() {
        let p = base + 48 * k;
        let body = p + 16;
        put(f, body, &(min + 1 + k).to_le_bytes());
        if k > 0 {
            put(f, p - 8, &field.to_le_bytes());
            put(f, p - 6, &44u16.to_le_bytes());
        }
        let crc = match inner {
            None => crc32c(&f[body as usize..e as usize]),
            Some(c) => {
                crc32c_shift(
                    crc32c(&f[body as usize..(body + 48) as usize]),
                    e - body - 48,
                ) ^ c
            }
        };
        inner = Some(crc);
        let h = BlockHeader {
            block_type: kvik_files::record::record_type::LOST_RANGE,
            flags: 0,
            body_len: (e - body) as u32,
            body_crc32c: crc,
        };
        put(f, p, &h.encode());
    }
}

/// The manifest of `h`, a crafted tail after it, then `hidden` valid records, synced but in the part of a window the
/// search passes.
fn crafted_with_hidden(h: &History, hidden: &[u64]) -> (Vec<u8>, u64, u64) {
    let mut bytes = manifest_bytes(&h.records);
    let last = h.records.last().unwrap().epoch;
    let valid_end = bytes.len() as u64;
    crafted_tail(&mut bytes, last, 2_000, 100_000);
    for &e in hidden {
        bytes.extend(Record::new(e, 0, T0, Body::Empty).encode().unwrap());
    }
    (bytes, last, valid_end)
}

fn rebuilt_epochs(bytes: &[u8], h: &History) -> (u64, Vec<u64>) {
    let fs = fs_with(bytes);
    let (_app, p) = rebuild_manifest(
        &fs,
        &dir(),
        STORE_ID,
        state(h),
        &FakeVerifier::default(),
        Finality::Provisional,
        NOW,
    )
    .unwrap();
    let rebuilt = read(&fs.read(&manifest_path()).unwrap(), &STORE_ID).unwrap();
    (
        p.rebuild_epoch,
        rebuilt.records().map(|r| r.epoch).collect(),
    )
}

/// Q-872: valid records the search passed in an open span keep their epochs; the rebuilt manifest takes none of them,
/// so no rebuilt record meets an uploaded one under its key.
#[test]
fn q872_a_rebuild_takes_no_epoch_of_a_valid_record_hidden_in_an_open_span() {
    let h = store();
    let last = h.records.last().unwrap().epoch;
    let (bytes, _, _) = crafted_with_hidden(&h, &[last + 1, last + 2]);
    let img = read(&bytes, &STORE_ID).unwrap();
    assert_eq!(img.corrupt.len(), 1);
    assert_eq!(img.corrupt[0].before_epoch, None);
    let (rebuild_epoch, epochs) = rebuilt_epochs(&bytes, &h);
    assert!(rebuild_epoch > last + 2);
    assert!(epochs.iter().all(|&e| e > last + 2), "{epochs:?}");
}

/// Q-872: a valid record the search passed bounds the rebuild's epochs even far above what the span's length could
/// hold.
#[test]
fn q872_a_hidden_record_far_above_the_spans_length_still_bounds_the_rebuilds_epochs() {
    let h = store();
    let far = h.records.last().unwrap().epoch + 1_000_000;
    let (bytes, last, valid_end) = crafted_with_hidden(&h, &[far]);
    let img = read(&bytes, &STORE_ID).unwrap();
    let span = img.corrupt[0];
    assert_eq!((span.after_epoch, span.named_epoch), (last, far));
    assert!(last + (bytes.len() as u64 - valid_end) / 40 < far);
    assert_eq!(span.epoch_ceiling(), far);
    let (rebuild_epoch, epochs) = rebuilt_epochs(&bytes, &h);
    assert_eq!(rebuild_epoch, far + 1);
    assert!(epochs.iter().all(|&e| e > far));
}

/// Q-650, Q-174: a span with no later record may hold any number of records, so a loss that no valid record names
/// behind it is restated under its first missing position (field 6).
#[test]
fn q650_a_loss_no_valid_record_names_behind_an_open_span_counts_in_field_6() {
    let (h, i) = lost_record_history();
    // The manifest up to the loss's own record; what follows it is a crafted tail.
    let mut bytes = manifest_bytes(&h.records[..i]);
    let last = h.records[i - 1].epoch;
    crafted_tail(&mut bytes, last, 2_000, 100_000);
    let img = read(&bytes, &STORE_ID).unwrap();
    assert_eq!(img.corrupt.len(), 1);
    assert_eq!(img.corrupt[0].before_epoch, None);
    let p = plan(&input(&img, &h), &FakeVerifier::default()).unwrap();
    assert_eq!(p.rebuild_record().unwrap().identities_rebuilt, 1);
}

/// Q-650: a copy written back for a span with no later record goes before the span, whose bytes are kept whole after
/// it: the passed part may hold valid records.
#[test]
fn q650_a_copy_written_back_keeps_an_open_spans_bytes_after_it() {
    let h = store();
    let (bytes, last, valid_end) = crafted_with_hidden(&h, &[]);
    let img = read(&bytes, &STORE_ID).unwrap();
    let copy = Record::new(last + 1, 0, T0, Body::Empty).encode().unwrap();
    assert!((copy.len() as u64) < bytes.len() as u64 - valid_end);
    let fs = fs_with(&bytes);
    repair_with_copy(&fs, &dir(), &img, &bytes, &copy).unwrap();
    let mut want = bytes[..valid_end as usize].to_vec();
    want.extend_from_slice(&copy);
    want.extend_from_slice(&bytes[valid_end as usize..]);
    assert_eq!(fs.read(&manifest_path()).unwrap(), want);
    let again = read(&want, &STORE_ID).unwrap();
    assert_eq!(again.last_epoch(), last + 1);
    assert_eq!(again.corrupt.len(), 1);
    assert_eq!(again.corrupt[0].before_epoch, None);
}
