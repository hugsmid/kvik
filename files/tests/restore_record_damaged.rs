//! A manifest of a generation above 0 whose only record, the restore record, is damaged is neither dropped as a
//! torn tail nor rebuilt as generation 0 (`spec.md` §7, §8 Generations).

mod support;

use kvik_files::appender::{Appender, Opened};
use kvik_files::catalogue::Tier;
use kvik_files::desc::{FileClass, FileDescription, kind};
use kvik_files::image::read;
use kvik_files::rebuild::{Finality, LocalState, rebuild_manifest};
use kvik_files::record::Record;
use kvik_files::restore::{RestoreRecord, RestoredFile};
use kvik_files::tier::DeepCopy;
use kvik_log::header::{FileHeader, kind as file_kind};
use kvik_log::mark::ResetStamp;
use support::*;

fn restore_record() -> RestoreRecord {
    RestoreRecord {
        generation: 2,
        stamp: ResetStamp {
            sequence: 1,
            nonce: 9,
        },
        source_generation: 1,
        restored_end: (1, 30),
        carried_ends: Vec::new(),
        losses: Vec::new(),
        acknowledgements: Vec::new(),
        entry_losses: Vec::new(),
        files: vec![RestoredFile {
            description: FileDescription {
                kind: kind::LOG_SEGMENT,
                class: FileClass::None,
                generation: 1,
                file_id: 4,
                range: Some(((1, 1), (1, 30))),
                hash: [4; 32],
                inputs: Vec::new(),
            },
            tier: Tier::Hot,
            copy: DeepCopy {
                size: 4096,
                sha256: [9; 32],
            },
        }],
        drill: false,
    }
}

/// A generation 2 manifest holding only its restore record, one body byte damaged.
fn damaged() -> Vec<u8> {
    let mut b = FileHeader::new(file_kind::MANIFEST, 2, 0, T0, STORE_ID)
        .encode()
        .to_vec();
    let at = b.len() + 40;
    b.extend(
        Record::new(1, 2, T0, restore_record().to_body())
            .encode()
            .unwrap(),
    );
    assert_eq!(read(&b, &STORE_ID).unwrap().records.len(), 1);
    b[at] ^= 0x40;
    b
}

#[test]
fn a_damaged_sole_restore_record_is_not_dropped_as_a_torn_tail_by_the_open() {
    let bytes = damaged();
    let fs = fs_with(&bytes);
    let opened = Appender::open(&fs, &dir(), STORE_ID, 0);
    let after = read_all(&fs, &manifest_path());
    assert_eq!(
        after.len(),
        bytes.len(),
        "the open truncated the record away"
    );
    assert!(
        !matches!(opened, Ok(Opened::Ready(_))),
        "a generation 2 manifest with no restore record opened ready"
    );
}

#[test]
fn a_damaged_sole_restore_record_is_not_rebuilt_as_generation_0() {
    let fs = fs_with(&damaged());
    let state = LocalState {
        files: &[],
        losses: &[],
        lease_epoch: 0,
        mark_file_id: 0,
    };
    let r = rebuild_manifest(
        &fs,
        &dir(),
        STORE_ID,
        state,
        &FakeVerifier::default(),
        Finality::Final,
        T0,
    );
    let header = FileHeader::decode(&read_all(&fs, &manifest_path())).unwrap();
    assert_eq!(
        header.generation, 2,
        "the manifest was rewritten as generation 0"
    );
    assert!(
        r.is_err(),
        "the rebuild of a manifest with no restore record succeeded"
    );
}
