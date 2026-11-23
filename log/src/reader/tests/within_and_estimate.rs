use super::parts_and_lookup::{names, reader, store};
use super::*;
use crate::facts::Value;

#[test]
fn a_lookup_within_a_budget_below_what_it_keeps_overflows_and_one_with_room_to_grow_finds_everything()
 {
    let (fs, log) = store(40);
    let wanted = [Wanted {
        series_id: 3,
        from: 0,
        to: 40,
    }];
    let mut r = reader(&fs, 3);
    let whole = r.lookup(&wanted, &|_, _, _| false).unwrap();
    assert!(!whole.overflowed && whole.complete[0]);
    let texts: u64 = whole
        .facts
        .iter()
        .map(|f| match &f.row.value {
            Value::Text(s) => s.len() as u64,
            _ => 0,
        })
        .sum();
    let bytes = (whole.facts.capacity() * std::mem::size_of::<FoundFact>()) as u64 + texts;
    let within = |budget| Within {
        budget,
        ..Within::default()
    };
    let short = r
        .lookup_within(&wanted, &|_, _, _| false, within(bytes - 1))
        .unwrap();
    assert!(short.overflowed);
    assert!(short.facts.len() < whole.facts.len());
    // Room for the vector's last move too: its old buffer and its new one.
    let exact = r
        .lookup_within(&wanted, &|_, _, _| false, within(bytes * 3 / 2))
        .unwrap();
    assert!(!exact.overflowed);
    assert_eq!(exact.facts, whole.facts);
    log.shutdown().unwrap();
}

#[test]
fn a_lookup_within_positions_keeps_only_the_facts_between_them() {
    let (fs, log) = store(40);
    let wanted = [Wanted {
        series_id: 3,
        from: 0,
        to: 40,
    }];
    let mut r = reader(&fs, 3);
    let whole = r.lookup(&wanted, &|_, _, _| false).unwrap();
    assert!(whole.facts.len() > 4);
    let (a, z) = (whole.facts[1].position, whole.facts[3].position);
    let some = r
        .lookup_within(
            &wanted,
            &|_, _, _| false,
            Within {
                above: Some(a),
                upto: Some(z),
                ..Within::default()
            },
        )
        .unwrap();
    assert!(some.complete[0] && !some.overflowed);
    assert_eq!(some.facts, whole.facts[2..=3].to_vec());
    log.shutdown().unwrap();
}

#[test]
fn an_estimate_counts_what_a_lookup_finds_from_seal_indexes_and_the_tallies_of_unsealed_segments() {
    let (fs, log) = store(200);
    let mut r = reader(&fs, 3);
    let wanted = [
        Wanted {
            series_id: 7,
            from: 50,
            to: 149,
        },
        Wanted {
            series_id: 11,
            from: 0,
            to: 199,
        },
    ];
    assert_eq!(r.estimate(&wanted), None);
    let found = r.lookup(&wanted, &|_, _, _| false).unwrap();
    let n = found.facts.len() as u64;
    assert_eq!(n, 300);
    assert!(names(&fs).iter().any(|s| !s.sealed) && names(&fs).iter().any(|s| s.sealed));
    let e = r.estimate(&wanted).unwrap();
    assert!(e.abs_diff(n) <= n / 20, "{e} for {n}");
    let none = Wanted {
        series_id: 500,
        from: 0,
        to: 199,
    };
    assert_eq!(r.estimate(&[none]), Some(0));
    log.shutdown().unwrap();
}

#[test]
fn an_estimate_over_a_range_only_the_unsealed_segment_holds_counts_its_tally() {
    let (fs, log) = store(200);
    let mut r = reader(&fs, 3);
    let all = Wanted {
        series_id: 11,
        from: 0,
        to: 199,
    };
    r.lookup(&[all], &|_, _, _| false).unwrap();
    let sealed_to = r
        .sealed_max_t(all.series_id)
        .expect("a sealed segment holding the series");
    let from = sealed_to + 1;
    let tail = Wanted { from, ..all };
    let n = r.lookup(&[tail], &|_, _, _| false).unwrap().facts.len() as u64;
    assert!(n >= 3, "{n} facts in the open segment");
    let e = r.estimate(&[tail]).unwrap();
    assert!(e.abs_diff(n) <= n / 20, "{e} for {n}");
    log.shutdown().unwrap();
}

#[test]
fn room_for_the_expected_facts_is_taken_at_once_so_a_budget_doubling_overflows_holds_them() {
    let (fs, log) = store(60);
    let wanted = [Wanted {
        series_id: 5,
        from: 0,
        to: 59,
    }];
    let mut r = reader(&fs, 3);
    let whole = r.lookup(&wanted, &|_, _, _| false).unwrap();
    let n = whole.facts.len() as u64;
    assert_eq!(n, 60);
    let budget = n * std::mem::size_of::<FoundFact>() as u64;
    let doubling = r
        .lookup_within(
            &wanted,
            &|_, _, _| false,
            Within {
                budget,
                ..Within::default()
            },
        )
        .unwrap();
    assert!(doubling.overflowed);
    let expected = r
        .lookup_within(
            &wanted,
            &|_, _, _| false,
            Within {
                budget,
                expect: n,
                ..Within::default()
            },
        )
        .unwrap();
    assert!(!expected.overflowed);
    assert_eq!(expected.facts, whole.facts);
    assert_eq!(expected.facts.capacity() as u64, n);
    log.shutdown().unwrap();
}
