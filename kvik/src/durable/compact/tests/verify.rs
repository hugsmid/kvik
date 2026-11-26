use std::collections::{BTreeMap, BTreeSet};

use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::facts::Value;
use kvik_log::setaside::SetAside;

use super::super::VerifyError;
use super::super::run::{Part, place, take, write};
use super::super::verify::verify;
use super::{
    DAY, batch, input, put_segment, reading, restored_after_first, segment_bytes, temp_dir,
};

const T: i64 = 29 * DAY;

/// Writes the outputs of a run over one small segment after `tamper` has changed its placed rows, and verifies them.
fn verified(
    set_aside: &SetAside,
    tamper: impl FnOnce(&mut BTreeMap<(i64, i64), Part>),
) -> Result<usize, VerifyError> {
    let dir = temp_dir("verify");
    let segs = [
        batch(
            1,
            30 * DAY,
            vec![
                reading(1, T + 1, Value::F64(1)),
                reading(1, T + 2, Value::F64(2)),
                reading(2, T + 1, Value::F64(3)),
                Item::Entry(Box::new(Entry::Tzdata {
                    version: "2026a".into(),
                })),
            ],
        ),
        batch(5, 30 * DAY, vec![reading(1, T + 1, Value::F64(4))]),
    ];
    let c = put_segment(&dir, 0, 1, &segment_bytes(0, 1, &segs));
    let weeks = BTreeSet::new();
    let inp = input(&dir, set_aside, &weeks);
    let taken = take(&inp, &[c]).unwrap();
    let mut parts = place(&inp, &taken).unwrap();
    tamper(&mut parts);
    let written = write(&inp, &taken, parts).unwrap();
    let r = verify(&inp, &taken, &written).map(|o| o.len());
    std::fs::remove_dir_all(&dir).unwrap();
    r
}

fn day(start: i64) -> (i64, i64) {
    (start, DAY)
}

#[test]
fn untampered_outputs_verify() {
    assert_eq!(verified(&SetAside::new(vec![]), |_| {}).unwrap(), 3);
}

#[test]
fn a_dropped_row_fails_the_counts() {
    let e = verified(&SetAside::new(vec![]), |p| {
        p.get_mut(&day(T))
            .unwrap()
            .current
            .get_mut(&1)
            .unwrap()
            .pop();
    });
    assert!(matches!(e, Err(VerifyError::Counts)), "{e:?}");
}

#[test]
fn a_changed_value_fails_its_series_checksum() {
    let e = verified(&SetAside::new(vec![]), |p| {
        p.get_mut(&day(T)).unwrap().current.get_mut(&2).unwrap()[0].value ^= 1;
    });
    assert!(matches!(e, Err(VerifyError::Checksum(Some(2)))), "{e:?}");
}

#[test]
fn a_changed_entry_fails_the_entries_checksum() {
    let e = verified(&SetAside::new(vec![]), |p| {
        p.get_mut(&day(30 * DAY)).unwrap().entries[0].1 = Entry::Tzdata {
            version: "2026b".into(),
        };
    });
    assert!(matches!(e, Err(VerifyError::Checksum(None))), "{e:?}");
}

#[test]
fn a_row_outside_its_files_period_is_misplaced() {
    let e = verified(&SetAside::new(vec![]), |p| {
        let part = p.remove(&day(T)).unwrap();
        p.insert(day(T - DAY), part);
    });
    assert!(matches!(e, Err(VerifyError::Placement(_))), "{e:?}");
}

#[test]
fn an_entry_outside_its_files_period_is_misplaced() {
    let e = verified(&SetAside::new(vec![]), |p| {
        let entries = std::mem::take(&mut p.get_mut(&day(30 * DAY)).unwrap().entries);
        p.get_mut(&day(T)).unwrap().entries = entries;
    });
    assert!(matches!(e, Err(VerifyError::Placement(_))), "{e:?}");
}

#[test]
fn a_revision_row_that_outranks_the_current_one_is_misplaced() {
    let e = verified(&SetAside::new(vec![]), |p| {
        let part = p.get_mut(&day(T)).unwrap();
        let current = part.current.get_mut(&1).unwrap();
        let revision = part.revision.get_mut(&1).unwrap();
        std::mem::swap(&mut current[0], &mut revision[0]);
    });
    assert!(matches!(e, Err(VerifyError::Placement(_))), "{e:?}");
}

#[test]
fn a_set_aside_row_outside_a_set_aside_chunk_is_misplaced() {
    let e = verified(&restored_after_first(), |p| {
        let part = p.get_mut(&day(T)).unwrap();
        let aside = std::mem::take(&mut part.set_aside);
        for (s, rows) in aside {
            part.revision.entry(s).or_default().extend(rows);
        }
    });
    assert!(matches!(e, Err(VerifyError::Placement(_))), "{e:?}");
}

#[test]
fn a_row_not_set_aside_inside_a_set_aside_chunk_is_misplaced() {
    let e = verified(&SetAside::new(vec![]), |p| {
        let part = p.get_mut(&day(T)).unwrap();
        let rows = part.revision.remove(&1).unwrap();
        part.set_aside.insert(1, rows);
    });
    assert!(matches!(e, Err(VerifyError::Placement(_))), "{e:?}");
}
