//! Latest values in the cases the state alone cannot tell, and what a reader holds (roadmap 9.0c; Q-1004, Q-1006).

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::tests::{
    Rig, S, newest_directory, plain, restamp_directory, write_directory, write_directory_under,
};
use crate::config::LogConfig;
use crate::durable::reader::DurableReader;
use crate::env::{Fs, OpenMode};
use crate::error::QueryError;
use crate::health::LossKind;
use crate::read::Live;
use crate::time::Timestamp;
use crate::types::{Intent, Reading, Value};
use crate::{Reader, SeriesId};

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

impl Rig {
    /// Waits until the segment holding `bytes` is sealed; returns its path.
    fn wait_sealed_with(&self, bytes: &[u8]) -> std::path::PathBuf {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let holding = self.segments().into_iter().find(|p| {
                self.fs
                    .read(p)
                    .is_some_and(|b| b.windows(bytes.len()).any(|w| w == bytes))
            });
            if let Some(p) = holding
                .clone()
                .filter(|p| p.extension().is_some_and(|e| e == "kvl"))
            {
                return p;
            }
            assert!(Instant::now() < deadline, "not sealed: {holding:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn agrees(&self, r: &Reader, i: usize) {
        let want = self.model.reader().live(self.ids[i]).unwrap();
        assert_eq!(
            plain(r.live(self.ids[i]).unwrap()),
            plain(want),
            "series {i}"
        );
    }

    fn at(&self, secs: i64) -> Timestamp {
        Timestamp(self.clock.wall().0 + secs * S)
    }

    fn note(&self, secs: i64, t: &str) -> Reading {
        Reading::new(self.ids[2], self.at(secs), Value::Text(t.into()))
    }

    /// Shuts the store, writes a series directory through the last batch, and opens the store again.
    fn directory(&mut self) {
        self.shut();
        write_directory(&self.env, &self.dir, usize::MAX).expect("a batch");
        self.reopen();
    }

    /// Damages the first copy of `text` in every log segment.
    fn damage(&self, text: &[u8]) {
        for p in self.segments() {
            let bytes = self.fs.read(&p).unwrap();
            if let Some(at) = bytes.windows(text.len()).position(|w| w == text) {
                self.fs.poke(&p, at, b"X");
            }
        }
    }

    /// The one log segment holding `bytes`.
    /// Emits texts enough to fill a 16 MiB segment, which then seals by size, at times from `from` on.
    fn fill(&mut self, from: i64) {
        for round in 0..3 {
            let readings: Vec<Reading> = (0..26_000i64)
                .map(|k| {
                    let t = from + k + round * 30_000;
                    Reading::new(self.ids[2], Timestamp(t), Value::Text(format!("{k:0>255}")))
                })
                .collect();
            self.emit_readings(&readings, Intent::Normal);
        }
    }

    fn holding(&self, bytes: &[u8]) -> std::path::PathBuf {
        let mut found = self.segments().into_iter().filter(|p| {
            self.fs
                .read(p)
                .is_some_and(|b| b.windows(bytes.len()).any(|w| w == bytes))
        });
        let p = found.next().expect("a segment holding the bytes");
        assert!(found.next().is_none());
        p
    }

    fn segments(&self) -> Vec<std::path::PathBuf> {
        self.fs
            .paths()
            .into_iter()
            .filter(|p| {
                p.starts_with(self.dir.join("log"))
                    && p.extension().is_some_and(|e| e == "open" || e == "kvl")
            })
            .collect()
    }
}

#[test]
fn a_pending_fact_falling_due_after_the_directory_is_live_from_its_entry_with_no_search() {
    let mut rig = Rig::new("pending");
    // A scheduled series: a fact ahead of now is pending, not live.
    rig.emit(&[(1, -4, 0, 0, 0), (1, 30, 1, 0, 0)], false);
    rig.directory();
    let r = rig.reader();
    for secs in [0, 31, 60] {
        rig.clock.advance(Duration::from_secs(secs));
        rig.agrees(&r, 1);
        rig.agrees(&rig.long, 1);
    }
    let d = durable(&r);
    assert!(d.seeded());
    assert_eq!((d.searches(), durable(&rig.long).searches()), (0, 0));
    // Two pending facts: the entry names the earliest only, so the reader looks once the first falls due.
    rig.emit(&[(1, 30, 1, 0, 0), (1, 90, 0, 0, 0)], false);
    rig.directory();
    let r = rig.reader();
    for secs in [0, 31, 60] {
        rig.clock.advance(Duration::from_secs(secs));
        rig.agrees(&r, 1);
        rig.agrees(&rig.long, 1);
    }
    assert_eq!(durable(&rig.long).searches(), 0);
    rig.check().unwrap();
}

#[test]
fn a_bad_latest_fact_is_passed_over_and_a_candidate_corrected_to_bad_is_searched_for_once() {
    let mut rig = Rig::new("bad");
    rig.emit(&[(0, -30, 0, 0, 0), (0, -4, 1, 0, 0)], false);
    rig.emit(&[(0, -2, 0, 3, 0)], false);
    rig.agrees(&rig.long, 0);
    let long = durable(&rig.long);
    assert_eq!(long.searches(), 0);
    rig.emit(&[(0, -4, 0, 3, 0)], true);
    for _ in 0..3 {
        rig.agrees(&rig.long, 0);
    }
    assert_eq!(long.searches(), 1);
    // A fact folded since ends what was found.
    rig.emit(&[(0, -40, 1, 0, 0)], false);
    rig.agrees(&rig.long, 0);
    assert_eq!(long.searches(), 2);
    // A reader continuing the directory reads the entry's unsure mark.
    rig.directory();
    let r = rig.reader();
    rig.agrees(&r, 0);
    assert_eq!(durable(&r).searches(), 1);
    rig.check().unwrap();
}

#[test]
fn future_and_outranked_facts_are_live_only_when_current_and_due_with_no_search() {
    let mut rig = Rig::new("ahead");
    rig.emit(&[(0, -4, 0, 0, 0), (0, 30, 1, 0, 0)], false);
    rig.emit(&[(3, -4, 0, 0, 2)], false);
    rig.emit(&[(3, -4, 1, 0, 1)], false);
    for secs in [0, 31] {
        rig.clock.advance(Duration::from_secs(secs));
        for i in [0, 3] {
            rig.agrees(&rig.long, i);
        }
    }
    assert_eq!(durable(&rig.long).searches(), 0);
    let Live::Value(v) = rig.long.live(rig.ids[3]).unwrap() else {
        panic!("a live value");
    };
    assert_eq!(v.fact.changed_at, Some(2));
    rig.check().unwrap();
}

#[test]
fn a_quiet_series_candidate_is_read_once_with_every_other_unmet_one() {
    let mut rig = Rig::new("quiet");
    rig.emit(
        &[(2, -300, 0, 0, 0), (0, -300, 0, 0, 0), (6, -300, 0, 0, 0)],
        false,
    );
    rig.clock.advance(Duration::from_secs(3_600));
    rig.directory();
    let r = rig.reader();
    let d = durable(&r);
    assert_eq!(text(&r.live(rig.ids[2]).unwrap()), Some("one"));
    assert_eq!(d.lookups(), 1);
    for _ in 0..3 {
        for i in [0, 2, 6] {
            rig.agrees(&r, i);
        }
    }
    assert!(d.seeded());
    assert_eq!((d.lookups(), d.searches()), (1, 0));
}

#[test]
fn a_readers_memory_does_not_grow_with_the_facts() {
    let mut rig = Rig::new("memory");
    let early = rig.reader();
    let series = [0usize, 1, 2, 6];
    let mut held = Vec::new();
    for calls in [20u64, 160] {
        while rig.clock.wall().0 < super::tests::T0 + i64::try_from(calls).unwrap() * 50 * S {
            let rs: Vec<_> = (0..50)
                .map(|k| (series[k % 4], -50 + k as i64, k as u8, (k / 4) as u8 % 3, 0))
                .collect();
            rig.emit(&rs, false);
            rig.clock.advance(Duration::from_secs(50));
        }
        let fresh = rig.reader();
        let mut seen = Vec::new();
        for r in [&early, &rig.long, &fresh] {
            for i in series {
                rig.agrees(r, i);
                r.series(rig.ids[i]).unwrap();
            }
            seen.push(durable(r).footprint());
        }
        held.push(seen);
    }
    for (small, large) in held[0].iter().zip(&held[1]) {
        assert_eq!(small.0, large.0, "series, full facts and entries held");
        assert!(!large.1, "no fact index");
        // The log reader keeps the segment it follows, up to the mark, and no more.
        let segment = usize::try_from(LogConfig::default().segment_size).unwrap();
        assert!(
            large.2 <= segment,
            "{} bytes kept by the log reader",
            large.2
        );
    }
    let (series_held, rows, _) = held[1][2].0;
    assert!(series_held == 4 && rows <= 3 * 4, "{:?}", held[1][2]);
}

#[test]
fn a_text_candidate_in_a_maximal_batch_is_read_whole_from_its_position() {
    let log = LogConfig {
        max_batch_bytes: 8 << 20,
        ..LogConfig::default()
    };
    let mut rig = Rig::with_log("maximal", log);
    let n = 26_000i64;
    let readings: Vec<Reading> = (0..n)
        .map(|k| {
            let t = Timestamp(rig.clock.wall().0 - (n - k) * 1_000_000);
            Reading::new(rig.ids[2], t, Value::Text(format!("{k:0>255}")))
        })
        .collect();
    rig.emit_readings(&readings, Intent::Normal);
    rig.agrees(&rig.long, 2);
    rig.directory();
    let r = rig.reader();
    let live = r.live(rig.ids[2]).unwrap();
    assert_eq!(text(&live), Some(format!("{:0>255}", n - 1).as_str()));
    rig.agrees(&r, 2);
    assert!(durable(&r).seeded());
    assert_eq!(durable(&r).lookups(), 1);
    let largest = rig
        .segments()
        .iter()
        .map(|p| rig.fs.read(p).unwrap().len())
        .max()
        .unwrap();
    assert!(largest > 7 << 20, "{largest} bytes");
}

#[test]
fn a_loss_found_while_open_breaks_the_state_shows_the_loss_and_never_the_lost_fact() {
    let mut rig = Rig::new("loss");
    let id: SeriesId = rig.ids[2];
    rig.emit_readings(&[rig.note(-30, "NOTE-AAA")], Intent::Normal);
    rig.emit_readings(&[rig.note(-4, "NOTE-BBB")], Intent::Normal);
    let store = rig.store.take().unwrap();
    let own = store.reader();
    assert_eq!(text(&own.live(id).unwrap()), Some("NOTE-BBB"));
    for p in rig.segments() {
        let bytes = rig.fs.read(&p).unwrap();
        if let Some(at) = bytes.windows(8).position(|w| w == b"NOTE-BBB") {
            rig.fs.poke(&p, at, b"X");
        }
    }
    let lost =
        |l: Vec<crate::LossReport>| l.iter().filter(|l| l.kind == LossKind::DataLost).count();
    assert_eq!(lost(store.losses()), 1);
    assert_eq!(text(&own.live(id).unwrap()), Some("NOTE-AAA"));
    assert_eq!(lost(own.losses()), 1);
    assert!(
        durable(&own).footprint().1,
        "the broken state answers from a fact index"
    );
    // A reader whose own pass finds the loss folded nothing from it.
    let fresh = rig.reader();
    assert_eq!(text(&fresh.live(id).unwrap()), Some("NOTE-AAA"));
    assert_eq!(lost(fresh.losses()), 1);
    assert!(!durable(&fresh).footprint().1);
    rig.store = Some(store);
}

#[test]
fn a_candidate_whose_segment_is_gone_is_reported_at_every_call_never_passed_over() {
    let log = LogConfig {
        segment_size: 16 << 20,
        max_batch_bytes: 8 << 20,
        ..LogConfig::default()
    };
    let mut rig = Rig::with_log("missing", log);
    let v = 2_222.5f64;
    rig.emit_readings(
        &[Reading::new(rig.ids[0], rig.at(-4), Value::F64(v))],
        Intent::Normal,
    );
    rig.fill(rig.at(-3).0 - 26_000);
    let sealed = rig.wait_sealed_with(&v.to_le_bytes());
    rig.directory();
    rig.emit_readings(&[rig.note(-2, "NOTE-NEW")], Intent::Normal);
    assert_ne!(rig.holding(b"NOTE-NEW"), sealed);
    let r = rig.reader();
    r.refresh().unwrap();
    assert!(durable(&r).seeded());
    rig.fs.remove_file(&sealed).unwrap();
    for _ in 0..3 {
        let e = r.live(rig.ids[0]);
        assert!(matches!(e, Err(QueryError::NotAvailable { .. })), "{e:?}");
    }
}

#[test]
fn damage_under_a_directorys_candidate_never_shows_the_lost_fact() {
    let mut rig = Rig::new("under");
    let id = rig.ids[2];
    rig.emit_readings(&[rig.note(-30, "NOTE-AAA")], Intent::Normal);
    rig.emit_readings(&[rig.note(-4, "NOTE-BBB")], Intent::Normal);
    rig.directory();
    rig.emit(&[(0, -2, 0, 0, 0)], false);
    for p in rig.segments() {
        let bytes = rig.fs.read(&p).unwrap();
        if let Some(at) = bytes.windows(8).position(|w| w == b"NOTE-BBB") {
            rig.fs.poke(&p, at, b"X");
        }
    }
    // The reader starts in the segment holding the directory's `through`, under the damage.
    let r = rig.reader();
    assert_eq!(text(&r.live(id).unwrap()), Some("NOTE-AAA"));
    let lost = r
        .losses()
        .iter()
        .filter(|l| l.kind == LossKind::DataLost)
        .count();
    assert_eq!(lost, 1);
    let d = durable(&r);
    assert!(
        d.seeded() && d.footprint().1,
        "seeded, then answered from a fact index"
    );
}

#[test]
fn a_candidate_above_the_readers_now_is_not_live() {
    let mut rig = Rig::new("behind");
    rig.emit(&[(0, -90, 0, 0, 0), (0, -4, 1, 0, 0)], false);
    // A reader whose clock is a minute behind the writer's.
    let mut env = rig.env.clone();
    env.clock = Arc::new(crate::env::ManualClock::new(rig.at(-60)));
    let r = Reader::open(crate::config::ReaderConfig::new(&rig.dir).env(env)).unwrap();
    let Live::Value(v) = r.live(rig.ids[0]).unwrap() else {
        panic!("a live value");
    };
    assert_eq!(v.fact.value, Value::F64(1.0), "{v:?}");
    assert_eq!(durable(&r).searches(), 1);
}

#[test]
fn a_loss_found_in_a_batch_already_folded_breaks_the_state_and_one_the_pass_finds_does_not() {
    let mut rig = Rig::new("folded");
    let id: SeriesId = rig.ids[2];
    rig.emit_readings(&[rig.note(-30, "NOTE-AAA")], Intent::Normal);
    rig.emit_readings(&[rig.note(-20, "NOTE-BBB")], Intent::Normal);
    rig.emit(&[(0, -4, 0, 0, 0)], false);
    let store = rig.store.take().unwrap();
    let own = store.reader();
    assert_eq!(text(&own.live(id).unwrap()), Some("NOTE-BBB"));
    for p in rig.segments() {
        let bytes = rig.fs.read(&p).unwrap();
        if let Some(at) = bytes.windows(8).position(|w| w == b"NOTE-BBB") {
            rig.fs.poke(&p, at, b"X");
        }
    }
    // The loss watch finds it; the store's own reader read the batch before it was damaged.
    let lost =
        |l: Vec<crate::LossReport>| l.iter().filter(|l| l.kind == LossKind::DataLost).count();
    assert_eq!(lost(store.losses()), 1);
    assert_eq!(text(&own.live(id).unwrap()), Some("NOTE-AAA"));
    assert!(
        durable(&own).footprint().1,
        "the broken state answers from a fact index"
    );
    let fresh = rig.reader();
    assert_eq!(text(&fresh.live(id).unwrap()), Some("NOTE-AAA"));
    assert_eq!(lost(fresh.losses()), 1);
    assert!(
        !durable(&fresh).footprint().1,
        "a range its own pass found breaks nothing"
    );
    rig.store = Some(store);
}

#[test]
fn a_series_directory_under_another_reset_stamp_is_not_continued() {
    let mut rig = Rig::new("stamp");
    rig.emit(&[(0, -4, 0, 0, 0)], false);
    rig.shut();
    let other = kvik_log::mark::ResetStamp {
        sequence: 1,
        nonce: 7,
    };
    write_directory_under(&rig.env, &rig.dir, usize::MAX, Some(other)).unwrap();
    let r = rig.reader();
    rig.agrees(&r, 0);
    assert!(!durable(&r).seeded());
    write_directory(&rig.env, &rig.dir, usize::MAX).unwrap();
    let r = rig.reader();
    rig.agrees(&r, 0);
    assert!(durable(&r).seeded());
    rig.reopen();
}

#[test]
fn a_continuing_reader_never_shows_a_fact_a_start_recorded_lost() {
    let mut rig = Rig::new("recorded");
    rig.emit(&[(0, -60, 0, 0, 0), (2, -60, 0, 0, 0)], false);
    rig.directory();
    let r = rig.reader();
    r.refresh().unwrap();
    assert!(durable(&r).seeded());
    rig.emit_readings(&[rig.note(-30, "NOTE-AAA")], Intent::Normal);
    rig.emit_readings(&[rig.note(-20, "NOTE-BBB")], Intent::Normal);
    rig.emit(&[(0, -4, 1, 0, 0)], false);
    assert_eq!(text(&r.live(rig.ids[2]).unwrap()), Some("NOTE-BBB"));
    rig.power_loss();
    rig.damage(b"NOTE-BBB");
    rig.reopen();
    let fresh = rig.reader();
    let lost = |l: &[crate::LossReport]| l.iter().filter(|l| l.kind == LossKind::DataLost).count();
    assert_eq!(lost(&fresh.losses()), 1);
    assert_eq!(text(&fresh.live(rig.ids[2]).unwrap()), Some("NOTE-AAA"));
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
fn a_reader_opened_before_the_newest_directory_continues_it() {
    let mut rig = Rig::new("opened-before");
    rig.emit(&[(0, -60, 0, 0, 0)], false);
    rig.directory();
    let r = rig.reader();
    rig.emit(&[(0, -30, 1, 0, 0), (2, -30, 0, 0, 0)], false);
    rig.directory();
    r.refresh().unwrap();
    assert!(durable(&r).seeded());
    for i in [0, 2] {
        rig.agrees(&r, i);
    }
    rig.check().unwrap();
}

#[test]
fn a_newest_directory_a_reader_cannot_continue_falls_back_to_the_one_kept_before_it() {
    let mut rig = Rig::new("fallback");
    rig.emit(&[(0, -60, 0, 0, 0)], false);
    rig.directory();
    rig.emit(&[(0, -30, 1, 0, 0)], false);
    rig.shut();
    let other = kvik_log::mark::ResetStamp {
        sequence: 1,
        nonce: 7,
    };
    write_directory_under(&rig.env, &rig.dir, usize::MAX, Some(other)).unwrap();
    rig.reopen();
    let r = rig.reader();
    r.refresh().unwrap();
    assert!(durable(&r).seeded());
    rig.agrees(&r, 0);
}

#[test]
fn a_directory_behind_a_loss_a_later_start_records_is_not_continued() {
    let mut rig = Rig::new("behind-loss");
    rig.emit_readings(&[rig.note(-30, "NOTE-AAA")], Intent::Normal);
    rig.emit_readings(&[rig.note(-20, "NOTE-BBB")], Intent::Normal);
    rig.emit(&[(0, -4, 1, 0, 0)], false);
    rig.directory();
    let folded = newest_directory(&rig.fs, &rig.dir);
    rig.power_loss();
    rig.damage(b"NOTE-BBB");
    rig.reopen();
    rig.shut();
    // Under the mark's reset stamp, so that only the count of lost positions tells it is behind the loss.
    restamp_directory(&rig.env, &rig.dir, &folded, 0);
    let r = rig.reader();
    assert_eq!(text(&r.live(rig.ids[2]).unwrap()), Some("NOTE-AAA"));
    assert!(!durable(&r).seeded());
}

#[test]
fn a_value_ahead_the_state_cannot_name_is_found_once_until_the_next_falls_due() {
    let mut rig = Rig::new("next-value");
    // A scheduled series given four values, three of them ahead: the entry names the earliest only.
    rig.emit(
        &[
            (1, -4, 0, 0, 0),
            (1, 30, 1, 0, 0),
            (1, 60, 0, 0, 0),
            (1, 90, 1, 0, 0),
        ],
        false,
    );
    rig.directory();
    let readers = [rig.reader(), rig.store.as_ref().unwrap().reader()];
    rig.clock.advance(Duration::from_secs(31));
    for r in &readers {
        for _ in 0..5 {
            rig.agrees(r, 1);
        }
        assert_eq!(durable(r).searches(), 1);
    }
    rig.clock.advance(Duration::from_secs(30));
    for r in &readers {
        for _ in 0..5 {
            rig.agrees(r, 1);
        }
        assert_eq!(durable(r).searches(), 2);
    }
}

#[test]
fn the_writers_own_reader_reads_no_file_while_the_writer_has_made_nothing_durable_since() {
    let mut rig = Rig::new("unchanged");
    rig.emit(&[(0, -4, 0, 0, 0)], false);
    let own = rig.store.as_ref().unwrap().reader();
    rig.agrees(&own, 0);
    let d = durable(&own);
    let read = d.parts_taken();
    for _ in 0..5 {
        rig.agrees(&own, 0);
        own.snapshot().unwrap().live(rig.ids[0]).unwrap();
    }
    assert_eq!(d.parts_taken(), read);
    rig.emit(&[(0, -2, 1, 0, 0)], false);
    rig.agrees(&own, 0);
    assert!(d.parts_taken() > read);
    // Another process's reader reads the mark at every call.
    let other = rig.reader();
    rig.agrees(&other, 0);
    let read = durable(&other).parts_taken();
    rig.agrees(&other, 0);
    assert!(durable(&other).parts_taken() > read);
}

#[test]
fn a_directory_candidate_inside_a_recorded_lost_range_is_searched_for_not_unavailable() {
    let log = LogConfig {
        segment_size: 16 << 20,
        max_batch_bytes: 8 << 20,
        ..LogConfig::default()
    };
    let mut rig = Rig::with_log("candidate-lost", log);
    rig.emit_readings(&[rig.note(-30, "NOTE-AAA")], Intent::Normal);
    rig.emit_readings(&[rig.note(-20, "NOTE-BBB")], Intent::Normal);
    rig.emit(&[(0, -4, 1, 0, 0)], false);
    rig.directory();
    let folded = newest_directory(&rig.fs, &rig.dir);
    rig.power_loss();
    rig.damage(b"NOTE-BBB");
    rig.reopen();
    // Older texts, which leave the live value alone, seal the segment the start recorded the range in, so that a
    // start after a directory through the mark meets no damage.
    rig.fill(rig.at(-90).0);
    let sealed = rig.wait_sealed_with(b"OTE-BBB");
    rig.emit(&[(0, -3, 1, 0, 0)], false);
    assert_ne!(rig.holding(&rig.at(-3).0.to_le_bytes()), sealed);
    rig.shut();
    restamp_directory(&rig.env, &rig.dir, &folded, 1);
    let r = rig.reader();
    assert_eq!(text(&r.live(rig.ids[2]).unwrap()), Some("NOTE-AAA"));
    let d = durable(&r);
    assert!(d.seeded() && !d.footprint().1);
    assert_eq!(d.searches(), 1);
}

#[test]
fn a_candidate_read_at_its_position_is_never_taken_for_another_fact_at_its_time() {
    let mut rig = Rig::new("candidate-position");
    rig.emit(&[(3, -4, 0, 0, 2)], false);
    // Outranked and recorded: an older version at the same time, at a later position.
    rig.emit(&[(3, -4, 1, 0, 1)], false);
    rig.directory();
    let r = rig.reader();
    let Live::Value(v) = r.live(rig.ids[3]).unwrap() else {
        panic!("a live value");
    };
    assert_eq!(v.fact.changed_at, Some(2));
    let d = durable(&r);
    assert!(d.seeded());
    assert_eq!((d.lookups(), d.searches()), (1, 0));
}

#[test]
fn a_registry_file_of_another_generation_or_above_the_mark_is_passed_over() {
    use kvik_log::registry::{self, RegistryFile};
    let mut rig = Rig::new("registry-file");
    rig.emit(&[(0, -4, 1, 0, 0)], false);
    rig.directory();
    rig.shut();
    let fs = &*rig.env.fs;
    let store_id = kvik_log::store::read_descriptor(fs, &rig.dir)
        .unwrap()
        .store_id;
    let (newest, file) = registry::read_latest(fs, &rig.dir, &store_id)
        .unwrap()
        .expect("a registry file");
    // Without its definitions: a reader that took it would know no series.
    let bare = RegistryFile {
        entries: Vec::new(),
        max_series_id: 0,
        ..file.clone()
    };
    let forged = [
        RegistryFile {
            through: file.through + 1_000,
            ..bare.clone()
        },
        RegistryFile {
            generation: file.generation + 1,
            ..bare
        },
    ];
    for f in &forged {
        let id = newest + 1;
        let path = rig
            .dir
            .join("registry")
            .join(registry::file_name(f.generation, id));
        let out = fs.open(&path, OpenMode::CreateNew).unwrap();
        out.write_at(&registry::encode(f, store_id, id, 0).unwrap(), 0)
            .unwrap();
        drop(out);
        let r = rig.reader();
        assert!(r.live(rig.ids[0]).is_ok(), "{f:?}");
        assert!(durable(&r).seeded());
        fs.remove_file(&path).unwrap();
    }
}
