//! Every read entry point of the store's own reader notes a loss its refresh finds to the process's latch and no
//! longer shows the lost fact (Q-938), on the path a call takes once an earlier call has caught the reader up.

mod common;

use std::time::Duration;

use common::durable::{World, damage, data_lost, gauge, history, num};
use common::{at, block_on};
use kvik::{
    Aggregate, Outcome, Position, Resolution, Select, SeriesId, Start, Store, View, Window,
};

#[derive(Clone, Copy, Debug)]
enum Read {
    Facts,
    Live,
    At,
    Window,
    Buckets,
    Interval,
    Series,
    Refresh,
    Orphaned,
}

const READS: [Read; 9] = [
    Read::Facts,
    Read::Live,
    Read::At,
    Read::Window,
    Read::Buckets,
    Read::Interval,
    Read::Series,
    Read::Refresh,
    Read::Orphaned,
];

/// Makes one read of the kind given; its answer does not matter, only the refresh it starts with.
fn read(s: &Store, g: SeriesId, how: Read) {
    let r = s.reader();
    let _ = match how {
        Read::Facts => r.facts(g, at(-10)..at(10), View::History).map(drop),
        Read::Live => r.live(g).map(drop),
        Read::At => r.at(g, at(5), View::Current).map(drop),
        Read::Window => r
            .window(g, Window::absolute(at(0)..at(10)), Aggregate::Count)
            .map(drop),
        Read::Buckets => r
            .buckets(g, at(0)..at(10), Resolution::Step(Duration::from_secs(5)))
            .map(drop),
        Read::Interval => r.interval(g, at(1)).map(drop),
        Read::Series => r.series(g).map(drop),
        Read::Refresh => r.refresh().map(drop),
        Read::Orphaned => s
            .export_orphaned(Position::ORIGIN..=Position::new(0, u64::MAX))
            .map(drop),
    };
}

#[test]
fn every_read_entry_point_notes_a_loss_its_refresh_finds_and_its_replay_is_shown_once() {
    for how in READS {
        let w = World::new(&format!("read-{how:?}"));
        let s = w.open();
        let g = block_on(s.define_series(gauge("a/b/power"))).unwrap();
        // With a consumer each emit's batch is taken by the deliverer before the emit returns, so it is read intact.
        let c = block_on(s.consumer("c", Start::Now, Select::None)).unwrap();
        block_on(s.emit(&[num(g, 0, 1.0)])).unwrap();
        read(&s, g, how);
        let mut lost = Position::ORIGIN;
        for (k, v) in [(1, 1_111.5), (2, 2_222.5), (3, 3_333.5)] {
            let ack = block_on(s.emit(&[num(g, k, v)])).unwrap();
            if k == 2 {
                lost = ack.first().unwrap();
            }
        }
        // The loss watch passes the three batches intact; only the read below meets the damage.
        assert!(data_lost(&s.losses()).is_empty());
        damage(&w, &2_222.5f64.to_bits().to_le_bytes());
        read(&s, g, how);
        assert_eq!(
            data_lost(&s.losses()),
            vec![(lost, Some(lost), Some(lost), false)],
            "{how:?}"
        );
        let ack = block_on(s.emit(&[num(g, 2, 2_222.5)])).unwrap();
        assert!(
            matches!(
                ack.outcomes[..],
                [Outcome::Recorded {
                    unchecked: false,
                    ..
                }]
            ),
            "{how:?}: {:?}",
            ack.outcomes
        );
        let shown = history(&s.reader(), g);
        assert_eq!(shown.len(), 4, "{how:?}: {shown:?}");
        assert!(shown.iter().all(|f| f.position != lost), "{how:?}");
        drop(c);
        block_on(s.shutdown()).unwrap();
    }
}
