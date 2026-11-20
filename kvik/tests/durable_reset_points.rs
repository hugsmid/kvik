//! A recovery entry a start cannot read is still a reset point (`spec.md` §4 Resync, §11 Health; Q-1031, Q-1033):
//! the registry file keeps it when its sealed segment is damaged, removed or zeroed.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::block_on;
use common::durable::note;
use common::world::World;
use kvik::env::Fs;
use kvik::{DeliveryKind, Funnel, Kind, Position, Select, SeriesSpec, Start, Store};

fn segs(w: &World) -> Vec<(PathBuf, Vec<u8>)> {
    w.fs.paths()
        .into_iter()
        .filter(|p| {
            p.starts_with(&w.dir) && p.extension().is_some_and(|e| e == "open" || e == "kvl")
        })
        .filter_map(|p| Some((p.clone(), w.fs.read(&p)?)))
        .collect()
}

fn holding(w: &World, text: &[u8]) -> Vec<(PathBuf, Vec<u8>)> {
    segs(w)
        .into_iter()
        .filter(|(_, b)| b.windows(text.len()).any(|x| x == text))
        .collect()
}

fn nonzero_end(b: &[u8]) -> usize {
    b.iter().rposition(|x| *x != 0).map_or(0, |i| i + 1)
}

fn sealed_with(w: &World, text: &[u8]) -> PathBuf {
    for _ in 0..4_000 {
        let held = holding(w, text);
        if !held.is_empty()
            && held
                .iter()
                .all(|(p, _)| p.extension().is_some_and(|e| e == "kvl"))
        {
            return held[0].0.clone();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("not sealed");
}

fn open(w: &World, seal: Duration) -> Store {
    let mut c = w.config();
    c.log.seal_interval = seal;
    block_on(Store::open(c)).unwrap()
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Harm {
    None,
    /// Bytes inside the recovery entry's batch damaged.
    Entry,
    /// The sealed segment holding the entry deleted.
    Remove,
    /// The sealed segment's body zeroed.
    ZeroBody,
}

#[derive(Debug, PartialEq)]
enum Got {
    Fact(Position, String),
    Resync {
        at: Position,
        forget_above: Option<Position>,
    },
}

/// A consumer receives two facts above K (unconfirmed), a power loss takes them and the mark's updates, the start
/// writes a recovery entry at K + 1 and assigns K + 2 again, the segment is sealed and the store shut down cleanly,
/// then `harm` is done to that segment. Returns K, what the consumer receives at the next start, and the registry
/// files around that start.
fn history(harm: Harm) -> Run {
    let mut w = World::new(&format!("reset-{harm:?}"));
    let s = open(&w, Duration::from_secs(300));
    let id = block_on(s.define_series(SeriesSpec::new("a/b/note", Kind::Text, ""))).unwrap();
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    block_on(c.route(Funnel::facts(Select::All))).unwrap();
    let mut k = Position::ORIGIN;
    for (i, t) in ["P-ONE", "P-TWO"].iter().enumerate() {
        w.clock.advance(Duration::from_secs(1));
        k = block_on(s.emit(&[note(id, i as i64, t)]))
            .unwrap()
            .first()
            .unwrap();
        while let Some(d) = c.try_next().unwrap() {
            block_on(c.confirm(d.id)).unwrap();
        }
    }
    w.clock.advance(Duration::from_secs(2));
    let _ = s.stats();
    let mark_path = w.path_with("log/MARK").unwrap();
    let mark = w.fs.read(&mark_path).unwrap();
    let (seg_path, seg) = holding(&w, b"P-TWO").pop().unwrap();
    let end2 = nonzero_end(&seg);
    for (i, t) in ["P-THREE", "P-FOUR"].iter().enumerate() {
        block_on(s.emit(&[note(id, 2 + i as i64, t)])).unwrap();
    }
    let mut taken = 0;
    while let Some(d) = c.try_next().unwrap() {
        taken += usize::from(matches!(d.kind, DeliveryKind::Fact(_)));
    }
    assert_eq!(taken, 2);
    w.crash(s, vec![c], true);
    let end4 = nonzero_end(&w.fs.read(&seg_path).unwrap());
    w.fs.poke(&seg_path, end2, &vec![0u8; end4 - end2]);
    w.fs.poke(&mark_path, 0, &mark);

    let s = open(&w, Duration::from_millis(20));
    let end_entry = nonzero_end(&w.fs.read(&seg_path).unwrap_or_default());
    block_on(s.emit(&[note(id, 9, "FOUR-NEW")])).unwrap();
    let sealed = sealed_with(&w, b"P-TWO");
    sealed_with(&w, b"FOUR-NEW");
    block_on(s.shutdown()).unwrap();
    drop(s);
    match harm {
        Harm::None => {}
        Harm::Entry => w.fs.poke(&sealed, (end2 + end_entry) / 2, b"XXXX"),
        Harm::Remove => Fs::remove_file(&*w.fs, &sealed).unwrap(),
        Harm::ZeroBody => {
            let len = w.fs.read(&sealed).unwrap().len();
            let h = kvik_log::header::HEADER_LEN;
            w.fs.poke(&sealed, h, &vec![0u8; len - h]);
        }
    }
    let before = registry_files(&w);
    let s = open(&w, Duration::from_secs(300));
    let at_start = registry_files(&w);
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    let mut got = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        match &d.kind {
            DeliveryKind::Fact(f) => got.push(Got::Fact(f.position, format!("{:?}", f.value))),
            DeliveryKind::Resync(r) => got.push(Got::Resync {
                at: d.position,
                forget_above: r.forget_above,
            }),
            _ => {}
        }
        block_on(c.confirm(d.id)).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    drop(s);
    let s = open(&w, Duration::from_secs(300));
    let mut c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
    let mut again = Vec::new();
    while let Some(d) = c.try_next().unwrap() {
        if let DeliveryKind::Resync(r) = &d.kind {
            again.push(r.forget_above);
        }
        block_on(c.confirm(d.id)).unwrap();
    }
    block_on(s.shutdown()).unwrap();
    drop(s);
    Run {
        k,
        got,
        again,
        rewritten_at_start: before != at_start,
        recovery_kept: recovery_entries(&w),
    }
}

struct Run {
    k: Position,
    got: Vec<Got>,
    /// The `forget_above` of each resync at a later start, after every delivery was confirmed.
    again: Vec<Option<Position>>,
    rewritten_at_start: bool,
    /// The sequences of the recovery entries in the registry file after the last shutdown.
    recovery_kept: Vec<u64>,
}

fn registry_files(w: &World) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> =
        w.fs.paths()
            .into_iter()
            .filter(|p| p.starts_with(&w.dir) && p.extension().is_some_and(|e| e == "reg"))
            .collect();
    v.sort();
    v
}

fn recovery_entries(w: &World) -> Vec<u64> {
    let dir = Path::new(&w.dir);
    let id = kvik_log::store::read_descriptor(&*w.fs, dir)
        .unwrap()
        .store_id;
    let (_, f) = kvik_log::registry::read_latest(&*w.fs, dir, &id)
        .unwrap()
        .unwrap();
    f.entries
        .iter()
        .filter(|(_, _, e)| e.is_recovery())
        .map(|(s, _, _)| *s)
        .collect()
}

/// Before any fact above K, the consumer is told to forget every position above K.
fn told_to_forget_first(harm: Harm) {
    let Run {
        k,
        got,
        again,
        rewritten_at_start,
        recovery_kept,
    } = history(harm);
    assert_eq!(
        recovery_kept,
        vec![k.sequence + 1],
        "{harm:?}: the registry file keeps the entry"
    );
    // A removed or zeroed segment also takes the definition and the routing change, so the file is written again.
    if matches!(harm, Harm::None | Harm::Entry) {
        assert!(
            !rewritten_at_start,
            "{harm:?}: the file passes the derived check"
        );
    }
    assert!(
        again.iter().all(Option::is_none),
        "{harm:?}: a confirmed reset point is not sent again: {again:?}"
    );
    let first_reset = got
        .iter()
        .position(|g| matches!(g, Got::Resync { forget_above: Some(f), .. } if *f <= k));
    let first_above = got
        .iter()
        .position(|g| matches!(g, Got::Fact(p, _) if *p > k));
    assert!(
        first_reset.is_some() && first_reset < first_above.or(Some(usize::MAX)),
        "{harm:?}: K = {k:?}, got {got:?}"
    );
}

#[test]
fn a_readable_recovery_entry_is_a_reset_point_at_the_next_start() {
    told_to_forget_first(Harm::None);
}

#[test]
fn a_recovery_entry_in_a_damaged_batch_stays_a_reset_point() {
    told_to_forget_first(Harm::Entry);
}

#[test]
fn a_recovery_entry_in_a_removed_segment_stays_a_reset_point() {
    told_to_forget_first(Harm::Remove);
}

#[test]
fn a_recovery_entry_in_a_zeroed_segment_stays_a_reset_point() {
    told_to_forget_first(Harm::ZeroBody);
}
