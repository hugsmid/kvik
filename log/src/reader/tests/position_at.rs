use super::parts_and_lookup::{fact, names, reader, store};
use super::*;

#[test]
fn a_position_at_a_time_passes_over_positions_a_later_recovery_entry_voids() {
    let (fs, log) = store(10);
    let before = log.stats().durable.sequence;
    log.submit(vec![fact(4, 1_000, 1), fact(4, 1_001, 1)])
        .unwrap()
        .wait()
        .unwrap();
    let (voided, at) = *commits(&fs).last().unwrap();
    assert!(voided.sequence > before);
    std::thread::sleep(Duration::from_millis(5));
    let r = crate::entries::Recovery {
        after: before,
        stamp: None,
        boot_id: [0; 16],
        tail_bytes: 0,
        readable_commits: 0,
        mark_state: crate::entries::MarkState::Readable,
        id_floor: 0,
        consumers: Vec::new(),
    };
    log.submit(vec![Item::from(crate::entries::Entry::Recovery(r))])
        .unwrap()
        .wait()
        .unwrap();
    log.submit(vec![
        fact(4, 1_000, 2),
        fact(4, 1_001, 2),
        fact(4, 1_002, 2),
    ])
    .unwrap()
    .wait()
    .unwrap();
    let got = reader(&fs, 93).position_at(at, &|_, _, _| false).unwrap();
    assert!(got.complete);
    assert_eq!(got.position.map(|p| p.sequence), Some(before));
    log.shutdown().unwrap();
}

/// Each batch's last position and commit time, as a refresh reads them.
fn commits(fs: &Arc<MemFs>) -> Vec<(Position, i64)> {
    reader(fs, 92)
        .refresh()
        .unwrap()
        .events
        .into_iter()
        .filter_map(|e| match e {
            ReadEvent::Batch(b) => Some((b.last, b.commit_time)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_position_at_a_time_is_the_last_batch_committed_by_then_in_every_segment() {
    let (fs, log) = store(60);
    let all = commits(&fs);
    let mut r = reader(&fs, 1);
    let mut times: Vec<i64> = all.iter().flat_map(|(_, c)| [*c - 1, *c, *c + 1]).collect();
    times.sort_unstable();
    times.dedup();
    for at in times {
        let want = all.iter().filter(|(_, c)| *c <= at).map(|(p, _)| *p).max();
        let got = r.position_at(at, &|_, _, _| false).unwrap();
        assert!(got.complete);
        assert_eq!(got.position, want, "at {at}");
    }
    log.shutdown().unwrap();
}

#[test]
fn a_position_at_a_time_inside_a_damaged_segment_is_incomplete_never_an_earlier_one() {
    let (fs, log) = store(60);
    let all = commits(&fs);
    let sealed: Vec<_> = names(&fs).into_iter().filter(|n| n.sealed).collect();
    let name = sealed[sealed.len() - 1];
    let mut r = reader(&fs, 2);
    let (first, last) = (
        r.read_trailer(name).unwrap().1.first_commit_time,
        r.read_trailer(name).unwrap().1.last_commit_time,
    );
    let inside = all
        .iter()
        .map(|(_, c)| *c)
        .find(|c| first <= *c && *c < last)
        .expect("a segment of several commit times");
    let path = Path::new("s")
        .join(crate::store::LOG_DIR)
        .join(name.to_string());
    let off = crate::header::HEADER_LEN + 40;
    let b = fs.read(&path).unwrap()[off];
    fs.poke(&path, off, &[b ^ 0x5A]);
    let got = r.position_at(inside, &|_, _, _| false).unwrap();
    assert!(!got.complete);
    let after = r.position_at(last, &|_, _, _| false).unwrap();
    assert!(
        after.complete,
        "a time the trailer decides needs no batch of the segment"
    );
    log.shutdown().unwrap();
}
