//! Parts and roots round-trip (`format-tree.md` T.13): every encoded file decodes to what was encoded, and every
//! accepted input encodes to itself.

mod files;

use files::*;
use kvik_tree::fuzz::{decode_part, decode_root, reframe};
use kvik_tree::part::{Entry, Part, Run, TARGET_BODY};
use kvik_tree::root::Root;
use kvik_tree::{EncodeError, Kind, MINUTE};
use proptest::prelude::*;

fn round_trip(p: &Part) -> Vec<u8> {
    let bytes = p.encode().unwrap();
    assert_eq!(&Part::decode(&bytes).unwrap(), p);
    decode_part(&bytes);
    bytes
}

#[test]
fn day_part_round_trips_and_splits_the_minute_run() {
    let bytes = round_trip(&day_part());
    let (_, blocks) = split(&bytes);
    let nodes: Vec<_> = blocks.iter().filter(|(t, _)| *t == 0x60).collect();
    assert!(nodes.len() >= 7, "{}", nodes.len());
    let minute: Vec<_> = nodes.iter().take_while(|(_, b)| b[1] == 1).collect();
    assert!(minute.len() >= 2);
    for (_, b) in &minute[..minute.len() - 1] {
        assert!(
            b.len() <= TARGET_BODY && b.len() > TARGET_BODY - 200,
            "{}",
            b.len()
        );
    }
}

#[test]
fn empty_part_round_trips() {
    let bytes = round_trip(&empty_part());
    assert_eq!(split(&bytes).1.len(), 2);
}

#[test]
fn reframing_a_valid_file_changes_nothing() {
    for bytes in [
        day_part().encode().unwrap(),
        sample_root().encode().unwrap(),
    ] {
        let mut again = bytes.clone();
        reframe(&mut again);
        assert_eq!(again, bytes);
    }
}

#[test]
fn directory_splits_past_1365_nodes_blocks() {
    let mut p = empty_part();
    p.runs = (0..3_000u64)
        .map(|s| Run {
            series: s * 1_000,
            level: 4,
            entries: vec![Entry {
                start: T0,
                stale: true,
                as_of: pos(3, s),
                node: None,
            }],
        })
        .collect();
    let bytes = round_trip(&p);
    let dirs = split(&bytes).1.iter().filter(|(t, _)| *t == 0x61).count();
    assert_eq!(dirs, 3);
}

#[test]
fn encoder_refuses_two_runs_of_one_series_and_level() {
    let mut p = day_part();
    let run = p.runs[0].clone();
    let (a, b) = run.entries.split_at(100);
    p.runs[0].entries = a.to_vec();
    p.runs.insert(
        1,
        Run {
            entries: b.to_vec(),
            ..run
        },
    );
    assert!(p.encode().is_err());
}

#[test]
fn encoder_refuses_an_entry_off_its_level() {
    let mut p = day_part();
    p.runs[1].entries[3].start += MINUTE;
    assert!(p.encode().is_err());
}

#[test]
fn encoder_refuses_a_level_outside_the_table() {
    let mut p = day_part();
    p.levels.remove(0);
    assert!(matches!(p.encode(), Err(EncodeError::Shape(_))));
}

#[test]
fn encoder_refuses_an_empty_run() {
    let mut p = empty_part();
    p.runs.push(Run {
        series: 1,
        level: 2,
        entries: Vec::new(),
    });
    assert!(p.encode().is_err());
}

#[test]
fn encoder_refuses_an_empty_node_stored_in_full() {
    let mut p = day_part();
    p.runs[1].entries[0].node = Some(kvik_tree::Node::empty(Kind::Gauge, T0, T0 + 15 * MINUTE));
    assert!(matches!(p.encode(), Err(EncodeError::Refused(_))));
}

#[test]
fn root_round_trips() {
    let r = sample_root();
    let bytes = r.encode().unwrap();
    assert_eq!(Root::decode(&bytes).unwrap(), r);
    decode_root(&bytes);
}

#[test]
fn every_prefix_of_a_part_and_a_root_is_refused() {
    for bytes in [
        day_part().encode().unwrap(),
        sample_root().encode().unwrap(),
    ] {
        for k in (0..bytes.len()).step_by(7) {
            assert!(Part::decode(&bytes[..k]).is_err());
            assert!(Root::decode(&bytes[..k]).is_err());
        }
    }
}

proptest! {
    #[test]
    fn generated_parts_round_trip(p in generated_part()) {
        let bytes = p.encode().unwrap();
        prop_assert_eq!(Part::decode(&bytes).unwrap(), p);
        decode_part(&bytes);
    }

    #[test]
    fn generated_roots_round_trip(r in generated_root()) {
        let bytes = r.encode().unwrap();
        prop_assert_eq!(Root::decode(&bytes).unwrap(), r);
        decode_root(&bytes);
    }
}

#[test]
fn a_nodes_block_takes_an_entry_that_fills_it_to_exactly_the_target() {
    let n = 13_103u64;
    let mut p = empty_part();
    p.period_len = n * MINUTE as u64;
    p.runs = vec![Run {
        series: 1,
        level: 1,
        entries: (0..n)
            .map(|i| Entry {
                start: T0 + i as i64 * MINUTE,
                stale: true,
                as_of: pos(3, if (1..5).contains(&i) { 200 } else { 0 }),
                node: None,
            })
            .collect(),
    }];
    let bytes = round_trip(&p);
    let bodies: Vec<_> = split(&bytes)
        .1
        .into_iter()
        .filter(|(t, _)| *t == 0x60)
        .collect();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0].1.len(), TARGET_BODY);
}
