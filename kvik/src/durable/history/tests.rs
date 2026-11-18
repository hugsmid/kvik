use kvik_log::facts::{FactFlags, ValueType};

use super::*;

fn fact(t: i64, call: u64, reference: bool) -> Known {
    Known::of(
        &FactRow {
            series_id: 1,
            t,
            quality: LogQuality::Good,
            flags: FactFlags {
                unsynced_clock: !reference,
                ..FactFlags::default()
            },
            value: LogValue::Absent {
                value_type: ValueType::F64,
                scale: 0,
            },
            changed_at: None,
        },
        call,
    )
}

fn note(h: &mut History, key: Key, k: Known) {
    h.note(key, k, || WINDOW_SPAN, None);
}

fn calls(f: Facts<'_>) -> Option<Vec<u64>> {
    match f {
        Facts::Known(v) => Some(v.iter().map(|k| k.call).collect()),
        Facts::Slow(_) => None,
    }
}

#[test]
fn a_known_fact_takes_no_more_room_with_its_reference_flag() {
    #[allow(dead_code)]
    struct Without {
        value: LogValue,
        supplied: LogQuality,
        changed_at: Option<u64>,
        call: u64,
    }
    assert_eq!(std::mem::size_of::<Known>(), std::mem::size_of::<Without>());
}

#[test]
fn forgetting_keeps_only_facts_in_flight_in_the_overlay_and_holds_no_bytes() {
    let mut h = History::default();
    note(&mut h, (1, 10), fact(10, 1, true));
    note(&mut h, (1, 20), fact(20, 2, true));
    note(&mut h, (2, 10), fact(10, 3, true));
    h.insert_revised((2, 5), vec![fact(5, 0, true), fact(5, 3, true)]);
    h.readable_below(3);
    h.forget_shown();
    let held = h.held();
    assert_eq!((held.bytes, held.window, held.revised_keys), (0, 0, 0));
    assert_eq!(held.overlay, 2, "{held:?}");
    assert_eq!(calls(h.facts((2, 10))), None);
    assert!(matches!(h.facts((2, 10)), Facts::Slow(v) if v.len() == 1 && v[0].call == 3));
    assert!(matches!(h.facts((1, 20)), Facts::Slow(v) if v.is_empty()));
}

#[test]
fn after_a_forgetting_keys_up_to_the_latest_t_are_read_and_newer_ones_are_held() {
    let mut h = History::default();
    note(&mut h, (1, 10), fact(10, 0, true));
    note(&mut h, (1, 20), fact(20, 0, true));
    h.forget_shown();
    assert_eq!(calls(h.facts((1, 21))), Some(vec![]));
    note(&mut h, (1, 30), fact(30, 4, true));
    assert_eq!(calls(h.facts((1, 30))), Some(vec![4]));
    assert_eq!(calls(h.facts((1, 20))), None);
    note(&mut h, (1, 15), fact(15, 5, true));
    assert_eq!(
        calls(h.facts((1, 15))),
        None,
        "a late fact stays out of the window"
    );
    assert_eq!(h.held().window, 1);
}

#[test]
fn a_series_whose_latest_t_is_the_greatest_time_is_emptied_by_a_forgetting_and_by_the_budget() {
    let mut h = History::default();
    note(&mut h, (1, i64::MAX), fact(i64::MAX, 0, true));
    h.forget_shown();
    assert_eq!(calls(h.facts((1, i64::MAX))), None);
    let mut h = History::with_budget(0);
    note(&mut h, (1, i64::MAX), fact(i64::MAX, 0, true));
    assert_eq!(h.held().window, 0);
    assert_eq!(calls(h.facts((1, i64::MAX))), None);
}

#[test]
fn the_reference_after_a_forgetting_is_at_most_the_old_one_and_at_least_the_highest_not_shown() {
    let mut h = History::default();
    note(&mut h, (1, 10), fact(10, 1, true));
    note(&mut h, (1, 30), fact(30, 2, true));
    note(&mut h, (1, 40), fact(40, 3, false));
    h.readable_below(2);
    h.forget_shown();
    assert_eq!(
        h.reference(1),
        Reference::AtMost {
            upper: 30,
            least: Some(30)
        }
    );
    note(&mut h, (1, 20), fact(20, 4, true));
    assert_eq!(
        h.reference(1),
        Reference::AtMost {
            upper: 30,
            least: Some(30)
        }
    );
    h.readable_below(5);
    h.forget_shown();
    assert_eq!(
        h.reference(1),
        Reference::AtMost {
            upper: 30,
            least: None
        },
        "shown facts may be lost: only those not shown count"
    );
    note(&mut h, (1, 35), fact(35, 6, true));
    assert_eq!(h.reference(1), Reference::Exact(Some(35)));
    assert_eq!(h.reference(2), Reference::Exact(None));
}

#[test]
fn a_key_revised_in_every_call_is_evicted_once_its_earlier_facts_are_in_the_log() {
    let budget = 1_000;
    let mut h = History::with_budget(budget);
    note(&mut h, (1, 100), fact(100, 0, true));
    note(
        &mut h,
        (1, 5_000_000_000_000),
        fact(5_000_000_000_000, 0, true),
    );
    for call in 1..=10_000u64 {
        h.readable_below(call);
        let mut k = fact(100, call, true);
        k.changed_at = Some(call);
        let whole = (call == 1).then(|| vec![fact(100, 0, true)]);
        h.note((1, 100), k, || WINDOW_SPAN, whole);
        note(&mut h, (2, call as i64), fact(call as i64, call, true));
        let held = h.held();
        assert!(held.bytes <= budget, "call {call}: {held:?}");
        assert!(held.overlay <= 2, "call {call}: {held:?}");
    }
}

#[test]
fn a_key_revised_in_every_call_beyond_the_budget_leaves_the_other_series_windows_and_reads_only_itself()
 {
    for budget in [1_000usize, 100_000] {
        let mut h = History::with_budget(budget);
        for id in 10..30u32 {
            for k in 0..10i64 {
                h.note(
                    (id, 1_000 + k),
                    fact(1_000 + k, 0, true),
                    || WINDOW_SPAN,
                    None,
                );
            }
        }
        note(&mut h, (1, 100), fact(100, 0, true));
        note(
            &mut h,
            (1, 5_000_000_000_000),
            fact(5_000_000_000_000, 0, true),
        );
        let before = h.held();
        let in_memory: Vec<u32> = (10..30u32)
            .filter(|&id| matches!(h.facts((id, 1_009)), Facts::Known(v) if !v.is_empty()))
            .collect();
        assert!(!in_memory.is_empty());
        let mut log: Vec<Known> = vec![fact(100, 0, true)];
        for call in 1..=2_000u64 {
            h.readable_below(call);
            let mut k = fact(100, call, true);
            k.changed_at = Some(call);
            // Only the hot key is read, and at most its whole history (Q-945).
            let whole = match h.facts((1, 100)) {
                Facts::Slow(_) => Some(log.clone()),
                Facts::Known(_) => None,
            };
            assert!(whole.as_ref().is_none_or(|w| w.len() as u64 <= call));
            h.note((1, 100), k.clone(), || WINDOW_SPAN, whole);
            k.call = 0;
            log.push(k);
            let held = h.held();
            assert!(held.bytes <= budget, "{held:?}");
            assert!(
                held.window + 3 >= before.window,
                "call {call}: {before:?} then {held:?}"
            );
            for &id in &in_memory {
                assert!(
                    matches!(h.facts((id, 1_009)), Facts::Known(v) if !v.is_empty()),
                    "series {id} at call {call}"
                );
            }
        }
    }
}
