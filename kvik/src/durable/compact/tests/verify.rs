use std::collections::{BTreeMap, BTreeSet};

use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::facts::Value;
use kvik_log::setaside::SetAside;

use super::super::VerifyError;
use super::super::reference::{Part, place, take, verified_or_removed, write};
use super::super::{RunError, fact_digest};
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
    let r = verified_or_removed(&inp, &taken, &written).map(|o| o.len());
    let left = written.iter().filter(|w| w.path.exists()).count();
    assert_eq!(
        left,
        if r.is_ok() { written.len() } else { 0 },
        "a failed check removes every output"
    );
    std::fs::remove_dir_all(&dir).unwrap();
    r.map_err(|e| match e {
        RunError::Verify(v) => v,
        other => panic!("not a verification failure: {other:?}"),
    })
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

#[test]
fn a_row_moved_to_another_series_fails_the_counts() {
    let e = verified(&SetAside::new(vec![]), |p| {
        let part = p.get_mut(&day(T)).unwrap();
        let mut f = part.current.get_mut(&1).unwrap().pop().unwrap();
        f.series_id = 2;
        part.current.get_mut(&2).unwrap().push(f);
    });
    assert!(matches!(e, Err(VerifyError::Counts)), "{e:?}");
}

#[test]
fn every_field_of_a_fact_reaches_its_digest() {
    let base = kvik_chunk::file::Fact::value(1, (0, 1), 5, kvik_chunk::value_type::TEXT, 1);
    let d = |f: &kvik_chunk::file::Fact, r: i64, c: i64| fact_digest(f, r, c);
    let d0 = d(&base, 10, 20);
    let mut changed: Vec<u128> = vec![d(&base, 11, 20), d(&base, 10, 21)];
    let tweaks: [fn(&mut kvik_chunk::file::Fact); 10] = [
        |f| f.position.0 = 1,
        |f| f.position.1 = 2,
        |f| f.t = 6,
        |f| f.quality = 1,
        |f| f.flags = 1,
        |f| f.value_type = 1,
        |f| f.value = 2,
        |f| f.text = b"x".to_vec(),
        |f| f.changed_at = 1,
        |f| f.scale = 1,
    ];
    for t in tweaks {
        let mut f = base.clone();
        t(&mut f);
        changed.push(d(&f, 10, 20));
    }
    assert!(changed.iter().all(|&x| x != d0));
}
