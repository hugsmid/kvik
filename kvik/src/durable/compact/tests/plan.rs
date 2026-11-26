use std::collections::BTreeMap;
use std::path::PathBuf;

use kvik_files::desc::{FileClass, FileDescription};
use kvik_files::record::{Body, Record};
use kvik_log::fs::{Fs, OpenMode, StdFs};
use kvik_log::manifest::encode_initial;
use kvik_log::store::{LOG_DIR, MANIFEST};

use super::super::plan::{Candidate, Plan, candidates, plan};
use super::super::{Cause, Stall};
use super::{DAY, STORE, batch, put_segment, reading, segment_bytes, temp_dir};
use kvik_log::facts::Value;

fn cand(file_id: u64, first: u64, last: u64, len: u64, listed: bool) -> Candidate {
    let trailer = kvik_log::body::Trailer {
        entry_count: last - first + 1,
        first_sequence: first,
        last_sequence: last,
        first_commit_time: 0,
        last_commit_time: 0,
        batch_count: 1,
        index_offset: 0,
        file_crc32c: 0,
    };
    Candidate {
        generation: 0,
        file_id,
        path: PathBuf::new(),
        seal: Some((len, trailer)),
        listed,
    }
}

fn ids(p: &Plan) -> Vec<u64> {
    match p {
        Plan::Run(r) => r.iter().map(|c| c.file_id).collect(),
        _ => panic!("not a run: {p:?}"),
    }
}

fn three() -> [Candidate; 3] {
    [
        cand(1, 1, 10, 100, true),
        cand(2, 11, 20, 100, true),
        cand(3, 21, 30, 100, true),
    ]
}

#[test]
fn the_planner_starts_at_the_first_segment_above_c_g() {
    let cs = three();
    assert_eq!(ids(&plan(&cs, &BTreeMap::new(), 1000)), vec![1, 2, 3]);
    assert_eq!(
        ids(&plan(&cs, &BTreeMap::from([(0, 10)]), 1000)),
        vec![2, 3]
    );
    assert_eq!(
        ids(&plan(&cs, &BTreeMap::from([(1, 10)]), 1000)),
        vec![1, 2, 3]
    );
    assert_eq!(plan(&cs, &BTreeMap::from([(0, 30)]), 1000), Plan::Idle);
}

#[test]
fn the_planner_stops_once_its_inputs_reach_run_bytes_and_takes_at_least_one() {
    let cs = three();
    assert_eq!(ids(&plan(&cs, &BTreeMap::new(), 200)), vec![1, 2]);
    assert_eq!(ids(&plan(&cs, &BTreeMap::new(), 201)), vec![1, 2, 3]);
    assert_eq!(ids(&plan(&cs, &BTreeMap::new(), 0)), vec![1]);
}

#[test]
fn the_planner_stops_at_a_segment_the_manifest_does_not_list_yet() {
    let mut cs = three();
    cs[1].listed = false;
    assert_eq!(ids(&plan(&cs, &BTreeMap::new(), 1000)), vec![1]);
    assert_eq!(plan(&cs[1..], &BTreeMap::new(), 1000), Plan::Idle);
}

#[test]
fn the_planner_stalls_at_a_segment_straddling_c_g_or_without_a_trailer() {
    let cs = three();
    assert_eq!(
        plan(&cs, &BTreeMap::from([(0, 5)]), 1000),
        Plan::Stalled(Stall {
            generation: 0,
            file_id: 1,
            cause: Cause::Straddles
        })
    );
    assert_eq!(
        plan(&cs, &BTreeMap::from([(0, 15)]), 1000),
        Plan::Stalled(Stall {
            generation: 0,
            file_id: 2,
            cause: Cause::Straddles
        })
    );
    let mut broken = three();
    broken[1].seal = None;
    assert_eq!(ids(&plan(&broken, &BTreeMap::new(), 1000)), vec![1]);
    assert_eq!(
        plan(&broken, &BTreeMap::from([(0, 10)]), 1000),
        Plan::Stalled(Stall {
            generation: 0,
            file_id: 2,
            cause: Cause::Unreadable("no valid trailer")
        })
    );
}

#[test]
fn the_planner_takes_at_most_the_inputs_one_description_names() {
    let cs: Vec<Candidate> = (0..1100u64)
        .map(|i| cand(i + 1, i * 10 + 1, i * 10 + 10, 1, true))
        .collect();
    assert_eq!(ids(&plan(&cs, &BTreeMap::new(), u64::MAX)).len(), 1024);
}

#[test]
fn candidates_are_the_sealed_segments_in_position_order_marked_by_the_manifest() {
    let dir = temp_dir("cands");
    for (g, f) in [(1, 1), (0, 3), (0, 2)] {
        let b = segment_bytes(g, f, &[batch(1, DAY, vec![reading(1, 5, Value::F64(1))])]);
        put_segment(&dir, g, f, &b);
    }
    std::fs::write(
        dir.join(LOG_DIR).join("00000000-0000000000000009.open"),
        [0u8; 64],
    )
    .unwrap();
    let mut m = encode_initial(STORE, 0);
    let desc = FileDescription {
        kind: 1,
        class: FileClass::None,
        generation: 0,
        file_id: 3,
        range: Some(((0, 1), (0, 1))),
        hash: [0; 32],
        inputs: Vec::new(),
    };
    m.extend(
        Record::new(2, 0, 0, Body::Sealed(vec![desc]))
            .encode()
            .unwrap(),
    );
    StdFs
        .open(&dir.join(MANIFEST), OpenMode::CreateNew)
        .unwrap()
        .write_at(&m, 0)
        .unwrap();
    let cs = candidates(&StdFs, &dir, &STORE).unwrap();
    let got: Vec<(u32, u64, bool, bool)> = cs
        .iter()
        .map(|c| (c.generation, c.file_id, c.listed, c.seal.is_some()))
        .collect();
    assert_eq!(
        got,
        vec![(0, 2, false, true), (0, 3, true, true), (1, 1, false, true)]
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn without_a_manifest_no_candidate_is_listed() {
    let dir = temp_dir("nomanifest");
    let b = segment_bytes(0, 1, &[batch(1, DAY, vec![reading(1, 5, Value::F64(1))])]);
    put_segment(&dir, 0, 1, &b);
    let cs = candidates(&StdFs, &dir, &STORE).unwrap();
    assert_eq!(cs.len(), 1);
    assert!(!cs[0].listed);
    std::fs::remove_dir_all(&dir).unwrap();
}
