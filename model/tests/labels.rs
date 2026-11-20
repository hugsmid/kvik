//! Series labels (spec §2.1 Labels, §2.3 Label views): the reference model's
//! versions, views, withdrawals, duplicate rule, `declare`, limits at the call
//! only and orphaned label entries.

use std::collections::BTreeMap;

use kvik_model::labels::{
    Batch, Effective, LabelChange, LabelEntry, LabelError, LabelItem, LabelLimits, LabelLog,
    LabelView, MAX_ITEMS, MAX_KEYS_PER_SERIES, StoreLimit,
};
use kvik_model::registry::{Registry, RegistryEvent, SeriesOptions};
use kvik_model::types::{Kind, Nanos, Position, SeriesId, ValueType};

#[path = "labels/properties.rs"]
mod properties;

const S1: SeriesId = SeriesId(1);
const S2: SeriesId = SeriesId(2);
const TIMES: [Nanos; 11] = [i64::MIN, -5, 0, 5, 10, 15, 20, 25, 30, 35, 100];

fn registry(n: u32) -> Registry {
    let mut r = Registry::default();
    for i in 0..n {
        let ev = r
            .plan_define(
                &format!("plant/dev-{i}/temperature"),
                Kind::Gauge,
                ValueType::Number,
                "degC",
                None,
                SeriesOptions::default(),
                100,
            )
            .unwrap()
            .unwrap();
        r.apply(&ev);
    }
    r
}

type Call = (SeriesId, Effective, LabelChange);

struct Store {
    reg: Registry,
    log: LabelLog,
    limits: LabelLimits,
    next: Position,
    clock: Nanos,
}

impl Store {
    fn new(series: u32) -> Self {
        Store {
            reg: registry(series),
            log: LabelLog::new(),
            limits: LabelLimits::default(),
            next: Position::first(0),
            clock: 1_000,
        }
    }

    fn batch(&mut self) -> Batch {
        self.clock += 10;
        Batch {
            first: self.next,
            recorded_at: self.clock,
        }
    }

    fn advance(&mut self, r: &Result<kvik_model::labels::LabelOutcome, LabelError>) {
        if let Ok(Some(p)) = r.as_ref().map(|o| o.position) {
            self.next = p.next();
        }
    }

    fn many(&mut self, calls: &[Call]) -> Result<kvik_model::labels::LabelOutcome, LabelError> {
        let b = self.batch();
        let r = self.log.set_labels_many(&self.reg, &self.limits, calls, b);
        self.advance(&r);
        r
    }

    fn set(
        &mut self,
        s: SeriesId,
        from: Effective,
        change: LabelChange,
    ) -> Result<kvik_model::labels::LabelOutcome, LabelError> {
        self.many(&[(s, from, change)])
    }

    fn declare(
        &mut self,
        s: SeriesId,
        labels: &[(&str, &str)],
        now: Nanos,
    ) -> Result<kvik_model::labels::LabelOutcome, LabelError> {
        let b = self.batch();
        let r = self.log.declare(&self.reg, &self.limits, s, labels, now, b);
        self.advance(&r);
        r
    }

    fn at(&self, s: SeriesId, t: Nanos) -> BTreeMap<String, String> {
        self.log.labels_at(&self.reg, s, t, LabelView::Current)
    }
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn snapshot(log: &LabelLog, reg: &Registry, view: LabelView) -> Vec<BTreeMap<String, String>> {
    [S1, S2]
        .iter()
        .flat_map(|s| TIMES.iter().map(move |t| log.labels_at(reg, *s, *t, view)))
        .collect()
}

#[test]
fn s2_1_labels_a_change_takes_effect_from_its_time_and_a_correction_replaces_it() {
    let mut st = Store::new(1);
    st.set(
        S1,
        Effective::Always,
        LabelChange::new().set("room", "kitchen"),
    )
    .unwrap();
    st.set(
        S1,
        Effective::At(30),
        LabelChange::new().set("room", "hall"),
    )
    .unwrap();
    assert_eq!(st.at(S1, 29), labels(&[("room", "kitchen")]));
    assert_eq!(st.at(S1, 30), labels(&[("room", "hall")]));
    st.set(
        S1,
        Effective::At(30),
        LabelChange::new().set("room", "porch"),
    )
    .unwrap();
    assert_eq!(st.at(S1, 30), labels(&[("room", "porch")]));
    let h = st.log.history(&st.reg, S1);
    assert_eq!(
        h.iter().map(|v| v.superseded).collect::<Vec<_>>(),
        [false, true, false]
    );
}

#[test]
fn s2_1_labels_a_remove_makes_the_key_absent_from_its_time_on() {
    let mut st = Store::new(1);
    st.set(
        S1,
        Effective::Always,
        LabelChange::new().set("room", "kitchen"),
    )
    .unwrap();
    st.set(S1, Effective::At(10), LabelChange::new().remove("room"))
        .unwrap();
    st.set(
        S1,
        Effective::At(20),
        LabelChange::new().set("room", "hall"),
    )
    .unwrap();
    assert_eq!(st.at(S1, 9), labels(&[("room", "kitchen")]));
    assert_eq!(st.at(S1, 10), labels(&[]));
    assert_eq!(st.at(S1, 20), labels(&[("room", "hall")]));
}

#[test]
fn s2_1_labels_a_withdrawal_reads_as_if_no_change_was_made_and_a_later_set_reinstates_it() {
    let mut st = Store::new(1);
    st.set(
        S1,
        Effective::Always,
        LabelChange::new().set("room", "kitchen"),
    )
    .unwrap();
    st.set(
        S1,
        Effective::At(20),
        LabelChange::new().set("room", "hall"),
    )
    .unwrap();
    st.set(S1, Effective::At(20), LabelChange::new().withdraw("room"))
        .unwrap();
    assert_eq!(st.at(S1, 25), labels(&[("room", "kitchen")]));
    st.set(
        S1,
        Effective::At(10),
        LabelChange::new().set("room", "porch"),
    )
    .unwrap();
    assert_eq!(st.at(S1, 25), labels(&[("room", "porch")]));
    st.set(S1, Effective::At(20), LabelChange::new().remove("room"))
        .unwrap();
    assert_eq!(st.at(S1, 25), labels(&[]));
    assert_eq!(st.log.history(&st.reg, S1).len(), 5);
}

#[test]
fn s2_1_labels_a_future_change_takes_effect_when_event_time_reaches_it() {
    let mut st = Store::new(1);
    st.set(
        S1,
        Effective::At(1_000),
        LabelChange::new().set("tariff", "night"),
    )
    .unwrap();
    assert_eq!(st.log.labels(&st.reg, S1, 999), labels(&[]));
    assert_eq!(
        st.log.labels(&st.reg, S1, 1_000),
        labels(&[("tariff", "night")])
    );
}

#[test]
fn s2_1_labels_a_duplicate_is_only_the_same_item_at_the_same_key_and_effective_time() {
    let mut st = Store::new(1);
    st.set(
        S1,
        Effective::At(10),
        LabelChange::new().set("room", "hall"),
    )
    .unwrap();
    let o = st
        .set(
            S1,
            Effective::At(10),
            LabelChange::new().set("room", "hall").set("floor", "1"),
        )
        .unwrap();
    assert_eq!((o.recorded, o.duplicates), (1, 1));
    let o = st
        .set(
            S1,
            Effective::At(20),
            LabelChange::new().set("room", "hall"),
        )
        .unwrap();
    assert_eq!((o.recorded, o.duplicates), (1, 0));
    let o = st
        .set(
            S1,
            Effective::At(20),
            LabelChange::new().set("room", "hall"),
        )
        .unwrap();
    assert_eq!(
        o,
        kvik_model::labels::LabelOutcome {
            position: None,
            resolves_at: Some(Position::new(0, 3)),
            recorded: 0,
            duplicates: 1
        }
    );
    assert_eq!(st.log.versions(), 3);
}

#[test]
fn s2_1_labels_a_duplicate_counts_earlier_entries_of_the_same_batch() {
    let mut st = Store::new(1);
    let c = LabelChange::new().set("room", "hall");
    let o = st
        .many(&[
            (S1, Effective::Always, c.clone()),
            (S1, Effective::Always, c),
        ])
        .unwrap();
    assert_eq!((o.recorded, o.duplicates), (1, 1));
    assert_eq!(o.position, Some(Position::first(0)));
}

#[test]
fn s2_1_labels_a_call_resolves_at_its_last_entry_or_the_versions_it_duplicates() {
    let mut st = Store::new(2);
    st.set(S1, Effective::Always, LabelChange::new().set("a", "1"))
        .unwrap();
    st.set(S2, Effective::Always, LabelChange::new().set("a", "1"))
        .unwrap();
    st.set(S1, Effective::At(5), LabelChange::new().set("a", "2"))
        .unwrap();
    let o = st
        .many(&[
            (S1, Effective::Always, LabelChange::new().set("a", "1")),
            (S2, Effective::Always, LabelChange::new().set("a", "1")),
        ])
        .unwrap();
    assert_eq!(
        (o.position, o.resolves_at),
        (None, Some(Position::new(0, 2)))
    );
    let o = st
        .set(
            S1,
            Effective::At(5),
            LabelChange::new().set("a", "2").set("b", "1"),
        )
        .unwrap();
    assert_eq!(o.resolves_at, Some(Position::new(0, 4)));
    assert_eq!(o.resolves_at, o.position);
    let o = st.declare(S1, &[("a", "2")], 10).unwrap();
    assert_eq!(
        (o.position, o.resolves_at),
        (None, Some(Position::new(0, 3)))
    );
    let o = st.declare(S1, &[("a", "2"), ("b", "1")], 10).unwrap();
    assert_eq!(
        (o.position, o.resolves_at),
        (None, Some(Position::new(0, 4)))
    );
    let o = st.declare(S2, &[("a", "1"), ("c", "1")], 10).unwrap();
    assert_eq!(o.resolves_at, Some(Position::new(0, 5)));
    let o = st.set(S1, Effective::At(9), LabelChange::new()).unwrap();
    assert_eq!(o.resolves_at, None);
}

#[test]
fn s2_1_labels_a_bad_item_refuses_the_whole_call_and_appends_nothing() {
    let mut st = Store::new(2);
    let long = "a".repeat(64);
    let cases: Vec<(LabelChange, LabelError)> = vec![
        (
            LabelChange::new().set("Room", "x"),
            LabelError::BadKey { key: "Room".into() },
        ),
        (
            LabelChange::new().set("1room", "x"),
            LabelError::BadKey {
                key: "1room".into(),
            },
        ),
        (
            LabelChange::new().set("", "x"),
            LabelError::BadKey { key: "".into() },
        ),
        (
            LabelChange::new().set(&long, "x"),
            LabelError::BadKey { key: long.clone() },
        ),
        (
            LabelChange::new().set("area", "x"),
            LabelError::Reserved { key: "area".into() },
        ),
        (
            LabelChange::new().set("kvik_x", "x"),
            LabelError::Reserved {
                key: "kvik_x".into(),
            },
        ),
        (
            LabelChange::new().set("room", ""),
            LabelError::BadValue { key: "room".into() },
        ),
        (
            LabelChange::new().set("room", "a\u{7f}"),
            LabelError::BadValue { key: "room".into() },
        ),
        (
            LabelChange::new().set("room", "a\nb"),
            LabelError::BadValue { key: "room".into() },
        ),
        (
            LabelChange::new().set("room", "a\u{1f}"),
            LabelError::BadValue { key: "room".into() },
        ),
        (
            LabelChange::new().set("room", &"v".repeat(256)),
            LabelError::BadValue { key: "room".into() },
        ),
        (
            LabelChange::new().set("room", "a").remove("room"),
            LabelError::Duplicate { key: "room".into() },
        ),
        (
            (0..=MAX_ITEMS).fold(LabelChange::new(), |c, i| c.set(&format!("k{i}"), "v")),
            LabelError::TooManyItems,
        ),
    ];
    for (bad, err) in cases {
        let before = st.log.clone();
        let r = st.many(&[
            (S1, Effective::Always, LabelChange::new().set("ok", "1")),
            (S2, Effective::Always, bad),
        ]);
        assert_eq!(r, Err(err));
        assert_eq!(st.log, before);
    }
    assert_eq!(
        st.set(
            SeriesId(9),
            Effective::Always,
            LabelChange::new().set("ok", "1")
        ),
        Err(LabelError::UnknownSeries(SeriesId(9)))
    );
    let edge = "v".repeat(255);
    st.set(
        S1,
        Effective::Always,
        LabelChange::new()
            .set(&"k".repeat(63), &edge)
            .set("in", "\u{80} ü \"x\""),
    )
    .unwrap();
}

#[test]
fn s2_1_labels_an_empty_change_appends_nothing() {
    let mut st = Store::new(1);
    let o = st.set(S1, Effective::Always, LabelChange::new()).unwrap();
    assert_eq!(o.position, None);
    assert!(st.log.entries().is_empty());
}

#[test]
fn s2_1_labels_a_retired_series_keeps_and_still_takes_labels() {
    let mut st = Store::new(1);
    st.set(
        S1,
        Effective::Always,
        LabelChange::new().set("room", "hall"),
    )
    .unwrap();
    st.reg.apply(&RegistryEvent::Retire { id: S1, at: 50 });
    st.set(
        S1,
        Effective::At(10),
        LabelChange::new().set("room", "porch"),
    )
    .unwrap();
    assert_eq!(st.at(S1, 10), labels(&[("room", "porch")]));
}

#[test]
fn s2_1_labels_an_orphaned_entry_is_kept_and_counted_outside_every_view() {
    let mut st = Store::new(2);
    st.set(
        S2,
        Effective::Always,
        LabelChange::new().set("room", "hall"),
    )
    .unwrap();
    st.set(
        S1,
        Effective::Always,
        LabelChange::new().set("room", "porch"),
    )
    .unwrap();
    let kept = registry(1);
    let log = LabelLog::replay(st.log.entries().to_vec());
    assert_eq!(log.orphaned_entries(&kept), 1);
    assert_eq!(log.orphaned_entries(&st.reg), 0);
    assert_eq!(log.labels(&kept, S2, 0), labels(&[]));
    assert!(log.history(&kept, S2).is_empty());
    assert_eq!(
        log.label_values(&kept, "room", 0),
        BTreeMap::from([("porch".to_string(), 1)])
    );
    assert_eq!(log.versions(), 2);
    let tight = LabelLimits {
        versions: 2,
        ..LabelLimits::default()
    };
    let mut l = log.clone();
    let r = l.set_labels(
        &kept,
        &tight,
        S1,
        Effective::At(5),
        LabelChange::new().set("floor", "1"),
        Batch {
            first: Position::new(0, 3),
            recorded_at: 0,
        },
    );
    assert_eq!(
        r,
        Err(LabelError::StoreLimit {
            which: StoreLimit::Versions,
            limit: 2
        })
    );
}

#[test]
fn s2_1_labels_declare_sets_only_the_keys_whose_value_now_differs() {
    let mut st = Store::new(1);
    st.set(
        S1,
        Effective::Always,
        LabelChange::new().set("room", "hall").set("maker", "x1"),
    )
    .unwrap();
    st.set(
        S1,
        Effective::At(500),
        LabelChange::new().set("room", "porch"),
    )
    .unwrap();
    let o = st
        .declare(S1, &[("room", "hall"), ("type", "probe")], 100)
        .unwrap();
    assert_eq!((o.recorded, o.duplicates), (1, 1));
    assert_eq!(
        st.at(S1, 100),
        labels(&[("room", "hall"), ("type", "probe"), ("maker", "x1")])
    );
    assert_eq!(st.at(S1, 500)["room"], "porch");
    let o = st
        .declare(S1, &[("room", "hall"), ("type", "probe")], 200)
        .unwrap();
    assert_eq!((o.position, o.recorded, o.duplicates), (None, 0, 2));
    st.declare(S1, &[("room", "attic")], 300).unwrap();
    assert_eq!(st.at(S1, 299)["room"], "hall");
    assert_eq!(st.at(S1, 300)["room"], "attic");
    assert_eq!(
        st.declare(S1, &[("room", "a"), ("room", "b")], 400),
        Err(LabelError::Duplicate { key: "room".into() })
    );
    assert_eq!(
        st.declare(SeriesId(7), &[("room", "a")], 400),
        Err(LabelError::UnknownSeries(SeriesId(7)))
    );
}

#[test]
fn s2_1_labels_a_series_uses_at_most_64_keys_ever() {
    let mut st = Store::new(1);
    let full =
        (0..MAX_KEYS_PER_SERIES).fold(LabelChange::new(), |c, i| c.set(&format!("k{i}"), "v"));
    st.set(S1, Effective::Always, full).unwrap();
    let all_removed =
        (0..MAX_KEYS_PER_SERIES).fold(LabelChange::new(), |c, i| c.remove(&format!("k{i}")));
    st.set(S1, Effective::At(1), all_removed).unwrap();
    let before = st.log.clone();
    assert_eq!(
        st.set(S1, Effective::At(2), LabelChange::new().set("other", "v")),
        Err(LabelError::TooManyKeys {
            limit: MAX_KEYS_PER_SERIES
        })
    );
    assert_eq!(st.log, before);
    st.set(S1, Effective::At(2), LabelChange::new().set("k0", "w"))
        .unwrap();

    let mut st = Store::new(1);
    let taken_back = (0..MAX_KEYS_PER_SERIES).fold(LabelChange::new(), |c, i| {
        if i % 2 == 0 {
            c.remove(&format!("k{i}"))
        } else {
            c.withdraw(&format!("k{i}"))
        }
    });
    st.set(S1, Effective::At(1), taken_back).unwrap();
    assert_eq!(st.at(S1, 1), labels(&[]));
    assert_eq!(
        st.set(S1, Effective::At(2), LabelChange::new().set("other", "v")),
        Err(LabelError::TooManyKeys {
            limit: MAX_KEYS_PER_SERIES
        })
    );
}

#[test]
fn s2_1_labels_the_series_and_store_limits_refuse_a_call_that_adds_past_them() {
    let mut st = Store::new(2);
    st.limits = LabelLimits {
        versions_per_series: 2,
        versions: 3,
        bytes: u64::MAX,
    };
    st.set(
        S1,
        Effective::Always,
        LabelChange::new().set("a", "1").set("b", "1"),
    )
    .unwrap();
    let before = st.log.clone();
    assert_eq!(
        st.set(S1, Effective::At(1), LabelChange::new().set("a", "2")),
        Err(LabelError::SeriesLimit { limit: 2 })
    );
    assert_eq!(st.log, before);
    st.set(S2, Effective::Always, LabelChange::new().set("a", "1"))
        .unwrap();
    let before = st.log.clone();
    assert_eq!(
        st.set(S2, Effective::At(1), LabelChange::new().set("a", "2")),
        Err(LabelError::StoreLimit {
            which: StoreLimit::Versions,
            limit: 3
        })
    );
    assert_eq!(st.log, before);
    st.limits.versions = u64::MAX;
    st.limits.bytes = st.log.bytes();
    assert_eq!(
        st.set(S2, Effective::At(1), LabelChange::new().set("a", "2")),
        Err(LabelError::StoreLimit {
            which: StoreLimit::Bytes,
            limit: st.limits.bytes
        })
    );
    assert_eq!(st.log, before);
}

#[test]
fn s2_1_labels_an_entry_is_measured_by_its_encoded_length() {
    let entry = |from, items: Vec<(&str, LabelItem)>| LabelEntry {
        position: Position::first(0),
        recorded_at: 0,
        series: S1,
        from,
        items: items.into_iter().map(|(k, i)| (k.to_string(), i)).collect(),
    };
    let set = |v: &str| LabelItem::Set(v.to_string());
    assert_eq!(
        entry(Effective::Always, vec![("room", set("hall"))]).bytes(),
        32
    );
    assert_eq!(
        entry(Effective::At(7), vec![("room", set("hall"))]).bytes(),
        48
    );
    assert_eq!(
        entry(Effective::At(7), vec![("room", LabelItem::Withdraw)]).bytes(),
        40
    );
    assert_eq!(
        entry(
            Effective::Always,
            vec![("ab", set("c")), ("d", LabelItem::Remove)]
        )
        .bytes(),
        40
    );
}

#[test]
fn s2_1_labels_keys_and_values_count_series_carrying_them_now() {
    let mut st = Store::new(2);
    st.set(
        S1,
        Effective::Always,
        LabelChange::new().set("room", "hall"),
    )
    .unwrap();
    st.set(
        S2,
        Effective::Always,
        LabelChange::new().set("room", "hall"),
    )
    .unwrap();
    st.set(
        S2,
        Effective::At(10),
        LabelChange::new().set("room", "porch"),
    )
    .unwrap();
    assert_eq!(
        st.log.label_keys(&st.reg, 5),
        BTreeMap::from([("room".to_string(), 2)])
    );
    assert_eq!(
        st.log.label_values(&st.reg, "room", 10),
        BTreeMap::from([("hall".to_string(), 1), ("porch".to_string(), 1)])
    );
}

#[test]
fn s2_3_label_views_as_of_time_takes_the_versions_committed_by_then() {
    let mut st = Store::new(1);
    st.set(
        S1,
        Effective::Always,
        LabelChange::new().set("room", "hall"),
    )
    .unwrap();
    let first = st.clock;
    st.set(
        S1,
        Effective::Always,
        LabelChange::new().set("room", "porch"),
    )
    .unwrap();
    let v = |t| st.log.labels_at(&st.reg, S1, 0, LabelView::AsOfTime(t));
    assert_eq!(v(first - 1), labels(&[]));
    assert_eq!(v(first), labels(&[("room", "hall")]));
    assert_eq!(v(first + 9), labels(&[("room", "hall")]));
    assert_eq!(v(first + 10), labels(&[("room", "porch")]));
}

#[test]
fn s2_1_labels_notices_name_limits_at_80_percent() {
    let mut st = Store::new(2);
    st.set(
        S1,
        Effective::Always,
        LabelChange::new()
            .set("a", "1")
            .set("b", "1")
            .set("c", "1")
            .set("d", "1"),
    )
    .unwrap();
    st.set(S2, Effective::Always, LabelChange::new().set("a", "1"))
        .unwrap();
    let limits = LabelLimits {
        versions_per_series: 5,
        versions: 7,
        bytes: 1 << 20,
    };
    let n = st.log.notices(&st.reg, &limits);
    assert_eq!((n.store, n.series), (vec![], vec![S1]));
    let n = st.log.notices(
        &st.reg,
        &LabelLimits {
            versions: 6,
            bytes: st.log.bytes(),
            ..limits
        },
    );
    assert_eq!(n.store, vec![StoreLimit::Versions, StoreLimit::Bytes]);
    let n = st.log.notices(&registry(1), &limits);
    assert_eq!(n.series, vec![S1]);
}
