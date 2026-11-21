//! A snapshot's cut and `now` (Q-1005): what it answers while facts arrive and the clock moves, and its end.

use std::time::Duration;

use super::tests::{Rig, plain};
use crate::config::ReaderConfig;
use crate::error::QueryError;
use crate::{Reader, Snapshot};

fn assert_owned<T: Clone + Send + Sync + 'static>() {}

#[test]
fn a_snapshot_answers_at_its_cut_and_now_while_facts_arrive_and_ends_once_its_cut_is_old() {
    assert_owned::<Snapshot>();
    let mut rig = Rig::new("snapshot");
    // Series 1 has three values ahead: once the first falls due its live fact is looked up at the cut.
    rig.emit(
        &[
            (0, -60, 0, 0, 0),
            (1, -4, 0, 0, 0),
            (1, 30, 1, 0, 0),
            (1, 60, 0, 0, 0),
            (1, 90, 1, 0, 0),
            (2, -60, 0, 0, 0),
        ],
        false,
    );
    rig.clock.advance(Duration::from_secs(31));
    let r = Reader::open(
        ReaderConfig::new(&rig.dir)
            .env(rig.env.clone())
            .cut_max_age(Duration::from_secs(600)),
    )
    .unwrap();
    let snap = r.snapshot().unwrap();
    let model = rig.model.reader();
    let want: Vec<_> = rig
        .ids
        .iter()
        .map(|id| plain(model.live(*id).unwrap()))
        .collect();
    let (state, now, losses) = (snap.state(), snap.now(), snap.losses().unwrap());
    let series: Vec<_> = [0usize, 2]
        .iter()
        .map(|i| snap.series(rig.ids[*i]).unwrap().latest_t)
        .collect();
    // Later facts at times up to `now`, series 1's at `now` (looked up after them, never taken), and the clock moves
    // on.
    rig.emit(
        &[(0, -1, 1, 0, 0), (1, 0, 0, 0, 0), (2, -1, 1, 0, 0)],
        false,
    );
    rig.clock.advance(Duration::from_secs(5));
    assert_ne!(
        plain(r.live(rig.ids[0]).unwrap()),
        want[0],
        "a fresh read sees the later fact"
    );
    for (id, want) in rig.ids.iter().zip(&want) {
        assert_eq!(&plain(snap.live(*id).unwrap()), want, "{id:?}");
    }
    let again: Vec<_> = [0usize, 2]
        .iter()
        .map(|i| snap.series(rig.ids[*i]).unwrap().latest_t)
        .collect();
    assert_eq!(again, series);
    assert_eq!((snap.state(), snap.now()), (state, now));
    assert_eq!(snap.losses().unwrap(), losses);
    rig.clock.advance(Duration::from_secs(601));
    assert!(matches!(snap.live(rig.ids[0]), Err(QueryError::CutExpired)));
    assert!(matches!(
        snap.series(rig.ids[0]),
        Err(QueryError::CutExpired)
    ));
    assert!(matches!(snap.losses(), Err(QueryError::CutExpired)));
}

#[test]
fn a_snapshot_whose_now_is_before_a_kept_search_searches_again() {
    let rig = Rig::new("snapshot-earlier");
    let mut rig = rig;
    rig.emit(
        &[
            (1, -4, 0, 0, 0),
            (1, 30, 1, 0, 0),
            (1, 60, 0, 0, 0),
            (1, 90, 1, 0, 0),
        ],
        false,
    );
    rig.clock.advance(Duration::from_secs(31));
    let r = Reader::open(
        ReaderConfig::new(&rig.dir)
            .env(rig.env.clone())
            .cut_max_age(Duration::from_secs(600)),
    )
    .unwrap();
    let snap = r.snapshot().unwrap();
    let early = plain(rig.model.reader().live(rig.ids[1]).unwrap());
    rig.clock.advance(Duration::from_secs(30));
    let late = plain(rig.model.reader().live(rig.ids[1]).unwrap());
    assert_ne!(early, late);
    assert_eq!(plain(r.live(rig.ids[1]).unwrap()), late);
    assert_eq!(plain(snap.live(rig.ids[1]).unwrap()), early);
}
