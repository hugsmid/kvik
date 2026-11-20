//! Rules a continuing reader applies in every round, not only the first after its seed (Q-1001, Q-1006).

use std::sync::Arc;

use kvik_log::content::Item;
use kvik_log::entries::Entry;
use kvik_log::reader::{LogReader, ReadEvent, ReaderConfig as LogReaderConfig};

use super::tests::{Rig, S, newest_directory, plain, restamp_directory, write_directory};
use crate::Reader;
use crate::config::LogConfig;
use crate::durable::reader::DurableReader;
use crate::durable::tests::block_on;
use crate::health::LossKind;
use crate::read::Live;
use crate::time::Timestamp;
use crate::types::{Intent, Reading, Value};

fn durable(r: &Reader) -> Arc<DurableReader> {
    r.durable().expect("a durable reader").unwrap()
}

fn text(l: &Live) -> Option<&str> {
    match l {
        Live::Value(v) => match &v.fact.value {
            Value::Text(t) => Some(t),
            _ => None,
        },
        _ => None,
    }
}

fn note(rig: &mut Rig, secs: i64, t: &str) {
    let r = Reading::new(
        rig.ids[2],
        Timestamp(rig.clock.wall().0 + secs * S),
        Value::Text(t.into()),
    );
    rig.emit_readings(&[r], Intent::Normal);
}

fn damage(rig: &Rig, text: &[u8]) {
    for p in rig.fs.paths() {
        if !p.starts_with(rig.dir.join("log")) {
            continue;
        }
        let bytes = rig.fs.read(&p).unwrap();
        if let Some(at) = bytes.windows(text.len()).position(|w| w == text) {
            rig.fs.poke(&p, at, b"X");
        }
    }
}

fn directory(rig: &mut Rig) -> kvik_log::writer::Position {
    rig.shut();
    let through = write_directory(&rig.env, &rig.dir, usize::MAX).expect("a batch");
    rig.reopen();
    through
}

fn lost(r: &Reader) -> usize {
    r.losses()
        .iter()
        .filter(|l| l.kind == LossKind::DataLost)
        .count()
}

/// Fills the open segment past 16 MiB with texts of series 2 at times from `from` on, so that it seals by size.
fn fill(rig: &mut Rig, from: i64) {
    for round in 0..3 {
        let readings: Vec<Reading> = (0..26_000i64)
            .map(|k| {
                let t = from + k + round * 30_000;
                Reading::new(rig.ids[2], Timestamp(t), Value::Text(format!("{k:0>255}")))
            })
            .collect();
        rig.emit_readings(&readings, Intent::Normal);
    }
}

#[test]
fn a_range_recorded_in_a_later_round_breaks_a_state_seeded_with_one_held() {
    let log = LogConfig {
        segment_size: 16 << 20,
        max_batch_bytes: 8 << 20,
        ..LogConfig::default()
    };
    let mut rig = Rig::with_log("recorded-later", log);
    rig.emit(&[(0, -60, 0, 0, 0), (2, -60, 0, 0, 0)], false);
    note(&mut rig, -50, "NOTE-AAA");
    rig.emit(&[(0, -40, 1, 0, 0)], false);
    rig.power_loss();
    damage(&rig, b"NOTE-AAA");
    rig.reopen();
    // The recorded range stays in a sealed segment the continuing reader never scans.
    let from = rig.clock.wall().0 - 200 * S;
    fill(&mut rig, from);
    rig.emit(&[(0, -30, 0, 0, 0)], false);
    rig.shut();
    write_directory(&rig.env, &rig.dir, usize::MAX).expect("a batch");
    restamp_directory(&rig.env, &rig.dir, &newest_directory(&rig.fs, &rig.dir), 1);
    rig.reopen();
    let r = rig.reader();
    assert_eq!(lost(&r), 1);
    let d = durable(&r);
    assert!(d.seeded() && !d.footprint().1, "{} {:?}", d.seeded(), d.footprint());
    note(&mut rig, -20, "NOTE-BBB");
    note(&mut rig, -10, "NOTE-CCC");
    rig.emit(&[(0, -4, 1, 0, 0)], false);
    assert_eq!(text(&r.live(rig.ids[2]).unwrap()), Some("NOTE-CCC"));
    rig.power_loss();
    damage(&rig, b"NOTE-CCC");
    rig.reopen();
    let fresh = rig.reader();
    assert_eq!(lost(&fresh), 2);
    assert_eq!(text(&fresh.live(rig.ids[2]).unwrap()), Some("NOTE-BBB"));
    for id in &rig.ids {
        assert_eq!(
            plain(r.live(*id).unwrap()),
            plain(fresh.live(*id).unwrap()),
            "{id:?}"
        );
    }
    assert_eq!(r.losses(), fresh.losses());
}

#[test]
fn an_entry_a_registry_file_newer_than_the_directory_holds_is_not_applied_again() {
    use kvik_log::registry;
    let mut rig = Rig::new("registry-newer");
    rig.emit(&[(0, -4, 1, 0, 0)], false);
    let through = directory(&mut rig);
    let store = rig.store.as_ref().unwrap();
    block_on(store.alias(rig.ids[0], "h/g/watts")).unwrap();
    block_on(rig.model.alias(rig.ids[0], "h/g/watts")).unwrap();
    rig.emit(&[(0, -2, 0, 0, 0)], false);
    rig.shut();
    let fs = &*rig.env.fs;
    let store_id = kvik_log::store::read_descriptor(fs, &rig.dir)
        .unwrap()
        .store_id;
    let (_, file) = registry::read_latest(fs, &rig.dir, &store_id)
        .unwrap()
        .expect("a registry file");
    let mut log = LogReader::open(LogReaderConfig::new(
        &rig.dir,
        rig.env.fs.clone(),
        Arc::new(kvik_log::writer::SystemClock::default()),
        rig.env.boot.current(),
        [0xad; 16],
    ))
    .unwrap();
    let events = log.refresh().unwrap().events;
    let _ = log.close();
    let alias = events
        .iter()
        .filter_map(|e| match e {
            ReadEvent::Batch(b) => Some(b),
            _ => None,
        })
        .find_map(|b| {
            let mut sequence = b.first.sequence;
            for item in &b.content.items {
                if let Item::Entry(e) = item
                    && matches!(**e, Entry::Alias { .. })
                {
                    return Some((sequence, b.content.received_at, (**e).clone()));
                }
                sequence += item.sequences();
            }
            None
        })
        .expect("the alias entry");
    // The store's newest file, written at its shutdown, holds the alias the log returns above the directory.
    assert!(through.sequence < alias.0 && alias.0 <= file.through);
    assert!(file.entries.iter().any(|(s, _, _)| *s == alias.0));
    let r = rig.reader();
    let want = rig.model.reader().series(rig.ids[0]).unwrap().names;
    assert_eq!(want, ["h/g/power", "h/g/watts"]);
    assert_eq!(r.series(rig.ids[0]).unwrap().names, want);
    assert_eq!(r.series("h/g/watts").unwrap().id, rig.ids[0]);
    assert!(durable(&r).seeded());
    rig.reopen();
}
