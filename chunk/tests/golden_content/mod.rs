//! The content of each golden compacted segment (`tests/golden/compacted/README.md`), built from synthetic facts.

use kvik_chunk::file::{Class, Fact, Input, Level, Reach, Series};
use kvik_chunk::{flag, value_type};
use kvik_log::entries::{Entry, MarkState, Recovery};
use kvik_log::mark::ResetStamp;

use super::compacted::*;

pub fn series(id: u32, name: &str, facts: Vec<Fact>) -> Series {
    Series {
        series_id: id,
        name: Some(name.into()),
        reach: Reach::Point,
        facts,
        set_aside: vec![],
    }
}

/// Facts of `id` at positions `(generation, first + stride × i)`, a second apart from `T0`.
pub fn facts(id: u32, g: u32, first: u64, stride: u64, values: &[(u8, u64)]) -> Vec<Fact> {
    values
        .iter()
        .enumerate()
        .map(|(i, &(vt, v))| {
            Fact::value(
                id,
                (g, first + stride * i as u64),
                T0 + i as i64 * SECOND,
                vt,
                v,
            )
        })
        .collect()
}

pub fn of(vt: u8, values: impl IntoIterator<Item = u64>) -> Vec<(u8, u64)> {
    values.into_iter().map(|v| (vt, v)).collect()
}

pub fn xor_values(n: usize) -> Vec<u64> {
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let m = 1.0 + (x % (1 << 20)) as f64 / f64::from(1u32 << 20);
            (if x >> 63 == 1 { -m } else { m }).to_bits()
        })
        .collect()
}

pub fn l0_current() -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 300, 8);
    let words = ["idle", "", "heating", "dégivrage", "停止"];
    let mut note = facts(
        5,
        1,
        5,
        8,
        &of(
            value_type::TEXT,
            (0..300).map(|i| words[i % 5].len() as u64),
        ),
    );
    for (i, f) in note.iter_mut().enumerate() {
        f.text = words[i % 5].as_bytes().to_vec();
    }
    note[7] = Fact {
        flags: flag::NO_VALUE,
        value: 0,
        text: vec![],
        ..note[7].clone()
    };
    let mut counter = facts(
        2,
        1,
        2,
        8,
        &of(value_type::DECIMAL, (0..300).map(|i| i * 13)),
    );
    for f in &mut counter {
        f.scale = 3;
    }
    c.series = vec![
        series(
            1,
            "a/bool",
            facts(
                1,
                1,
                1,
                8,
                &of(value_type::BOOL, (0..300).map(|i| (i / 97) % 2)),
            ),
        ),
        series(2, "a/counter", counter),
        series(
            3,
            "a/mode",
            facts(
                3,
                1,
                3,
                8,
                &of(value_type::STATE, (0..300).map(|i| [0, 3, 65_535][i / 100])),
            ),
        ),
        series(
            4,
            "a/noise",
            facts(
                4,
                1,
                4,
                8,
                &of(value_type::F64, (0..300).map(|i| noise(i, 5))),
            ),
        ),
        series(5, "a/note", note),
        series(
            6,
            "a/temperature",
            facts(
                6,
                1,
                6,
                8,
                &of(
                    value_type::F64,
                    (0..300).map(|i| (20.0 + (i % 50) as f64 / 100.0).to_bits()),
                ),
            ),
        ),
        series(
            7,
            "a/vibration",
            facts(7, 1, 7, 8, &of(value_type::F64, xor_values(40))),
        ),
    ];
    c.settle()
}

pub fn level(level: Level, period_start: i64, period_len: i64) -> Content {
    let mut c = Content::new(Class::Current);
    c.description.level = level;
    c.description.period_start = period_start;
    c.description.period_len = period_len;
    c.description.inputs = vec![
        Input {
            kind: 2,
            file_id: 11,
        },
        Input {
            kind: 2,
            file_id: 12,
        },
    ];
    c.batches = batches(1, 1, 20, 2);
    c.series = vec![
        series(
            1,
            "l/a",
            facts(
                1,
                1,
                1,
                2,
                &of(value_type::F64, (0..20).map(|i| (i as f64 * 0.5).to_bits())),
            ),
        ),
        series(
            2,
            "l/b",
            facts(2, 1, 2, 2, &of(value_type::STATE, (0..20).map(|i| i % 3))),
        ),
    ];
    c.settle()
}

pub fn week_start() -> i64 {
    T0.div_euclid(7 * DAY) * 7 * DAY
}

pub fn revision() -> Content {
    let mut c = Content::new(Class::Revision);
    c.batches = batches(1, 1, 30, 1);
    let f = (0..30u64)
        .map(|i| f64_fact(1, (1, 1 + i), T0 + (i / 3) as i64 * SECOND, i as f64))
        .collect();
    c.series = vec![series(1, "r/forecast", f)];
    c.settle()
}

pub fn entries_two_generations() -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 20, 3);
    c.batches.extend(batches(2, 1, 20, 3));
    let mut f = Vec::new();
    for (n, b) in c.batches.clone().iter().enumerate() {
        f.push(f64_fact(
            1,
            (b.generation, b.first_sequence),
            T0 + n as i64 * SECOND,
            n as f64,
        ));
        if n % 5 == 0 {
            c.entries.push((
                (b.generation, b.first_sequence + 1),
                Entry::Alias {
                    id: 1,
                    name: format!("e/{n}"),
                },
            ));
            c.entries.push((
                (b.generation, b.first_sequence + 2),
                Entry::Retirement {
                    id: 1,
                    t_r: T0 + n as i64,
                },
            ));
        }
    }
    c.series = vec![series(1, "e/value", f)];
    c.settle()
}

pub fn entries_only() -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 4, 2);
    c.entries = (1..=8)
        .map(|q| {
            (
                (1, q),
                Entry::ScaleRaise {
                    id: 2,
                    scale: 3 + (q % 3) as u8,
                },
            )
        })
        .collect();
    c.settle()
}

pub fn recovery_entry() -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 3, 2);
    c.batches[2].first_sequence = 9;
    c.batches[2].last_sequence = 9;
    let recovery = Recovery {
        after: 4,
        stamp: Some(ResetStamp {
            sequence: 9,
            nonce: 0x5EED,
        }),
        boot_id: [4; 16],
        tail_bytes: 512,
        readable_commits: 1,
        mark_state: MarkState::Stale,
        id_floor: 7,
        consumers: vec!["export".into()],
    };
    c.entries = vec![((1, 9), Entry::Recovery(recovery))];
    c.series = vec![series(
        1,
        "x/a",
        facts(1, 1, 1, 1, &of(value_type::BOOL, [1, 0, 1, 1])),
    )];
    c.settle()
}

pub fn set_aside() -> Content {
    let mut c = Content::new(Class::Revision);
    c.batches = batches(1, 1, 12, 1);
    let mut s = series(
        1,
        "s/a",
        facts(
            1,
            1,
            1,
            1,
            &of(value_type::F64, (0..8).map(|i| f64::from(i).to_bits())),
        ),
    );
    s.set_aside = (0..4)
        .map(|i| f64_fact(1, (1, 9 + i), T0 + i as i64 * SECOND, 100.0))
        .collect();
    c.series = vec![s];
    c.settle()
}

pub fn decimals() -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 20, 2);
    let raise = (0..20i64)
        .map(|i| {
            decimal(
                1,
                (1, 1 + 2 * i as u64),
                T0 + i * SECOND,
                if i < 10 { 1_234 + i } else { 12_345 - i },
                if i < 10 { 2 } else { 3 },
            )
        })
        .collect();
    let overflow = (0..20i64)
        .map(|i| {
            decimal(
                2,
                (1, 2 + 2 * i as u64),
                T0 + i * SECOND,
                if i == 0 { i64::MAX / 2 } else { i },
                if i == 0 { 0 } else { 1 },
            )
        })
        .collect();
    c.series = vec![
        series(1, "d/raise", raise),
        series(2, "d/overflow", overflow),
    ];
    c.settle()
}

pub fn nan() -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 10, 2);
    let some = (0..10)
        .map(|i| {
            let v = if i == 4 {
                f64::from_bits(0x7FF8_0000_DEAD_BEEF)
            } else {
                f64::from(i) - 2.5
            };
            f64_fact(1, (1, 1 + 2 * i as u64), T0 + i64::from(i) * SECOND, v)
        })
        .collect();
    let all = (0..10u64)
        .map(|i| {
            Fact::value(
                2,
                (1, 2 + 2 * i),
                T0 + i as i64 * SECOND,
                value_type::F64,
                0x7FF8_0000_0000_0000 | i,
            )
        })
        .collect();
    c.series = vec![series(1, "n/some", some), series(2, "n/all", all)];
    c.settle()
}

pub fn text_and_orphan() -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 6, 2);
    let texts = ["on", "off", "", "standby", "on", "off"];
    let mut t: Vec<Fact> = texts
        .iter()
        .enumerate()
        .map(|(i, s)| text(1, (1, 1 + 2 * i as u64), T0 + i as i64 * SECOND, s))
        .collect();
    t[2] = Fact {
        flags: flag::NO_VALUE,
        value: 0,
        text: vec![],
        ..t[2].clone()
    };
    let mixed = vec![
        f64_fact(900, (1, 2), T0, 1.5),
        decimal(900, (1, 4), T0 + SECOND, 15, 1),
        Fact::value(900, (1, 6), T0 + 2 * SECOND, value_type::STATE, 2),
        text(900, (1, 8), T0 + 3 * SECOND, "x"),
        Fact::value(900, (1, 10), T0 + 4 * SECOND, value_type::BOOL, 1),
    ];
    c.series = vec![
        series(1, "t/text", t),
        Series {
            series_id: 900,
            name: None,
            reach: Reach::Point,
            facts: mixed,
            set_aside: vec![],
        },
    ];
    c.settle()
}

pub fn state_bool() -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 10, 2);
    c.series = vec![
        series(
            1,
            "b/state",
            facts(
                1,
                1,
                1,
                2,
                &of(value_type::STATE, [4, 2, 9, 2, 4, 7, 2, 9, 4, 3]),
            ),
        ),
        series(
            2,
            "b/bool",
            facts(2, 1, 2, 2, &of(value_type::BOOL, [1; 10])),
        ),
    ];
    c.settle()
}

pub fn interval_reach() -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 4, 3);
    let mut fixed = series(
        1,
        "i/energy",
        facts(1, 1, 1, 3, &of(value_type::DECIMAL, [10, 20, 30, 40])),
    );
    fixed.reach = Reach::Fixed(900 * SECOND);
    let mut day = series(
        2,
        "i/daily",
        facts(2, 1, 2, 3, &of(value_type::DECIMAL, [5, 6, 7, 8])),
    );
    day.reach = Reach::Day;
    let mut month = series(
        3,
        "i/monthly",
        facts(3, 1, 3, 3, &of(value_type::DECIMAL, [1, 2, 3, 4])),
    );
    month.reach = Reach::Month;
    c.series = vec![fixed, day, month];
    c.settle()
}

pub fn long_series() -> Content {
    let n = 64 * 4096 + 1;
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 65, 4096);
    c.series = vec![series(
        1,
        "long/a",
        facts(
            1,
            1,
            1,
            1,
            &of(value_type::STATE, (0..n).map(|i| (i / 50_000) as u64)),
        ),
    )];
    c.settle()
}

pub fn many_chunks() -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 1, 1025);
    c.series = (1..=1025u32)
        .map(|id| {
            series(
                id,
                &format!("m/{id:04}"),
                vec![Fact::value(id, (1, u64::from(id)), T0, value_type::BOOL, 1)],
            )
        })
        .collect();
    c.settle()
}

pub fn many_batches() -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 4097, 1);
    c.series = vec![series(
        1,
        "mb/a",
        facts(
            1,
            1,
            1,
            1,
            &of(value_type::BOOL, (0..4097).map(|i| (i / 7) % 2)),
        ),
    )];
    c.settle()
}

pub fn split_part() -> Content {
    let mut c = level(Level::Day, T0, DAY);
    c.description.name_range = Some(("l/a".into(), "l/b".into()));
    c
}

pub fn week_l0() -> Content {
    let mut c = level(Level::L0, week_start(), 7 * DAY);
    c.description.inputs = vec![Input {
        kind: 1,
        file_id: 3,
    }];
    c
}

pub fn xor_zstd() -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 2000, 1);
    let v = (0..2000).map(|i| (f64::from((i / 2 % 5) as u32) / 9.0 + 1.0).to_bits());
    c.series = vec![series(1, "z/a", facts(1, 1, 1, 1, &of(value_type::F64, v)))];
    c.settle()
}

pub fn tweaked(f: impl FnOnce(&mut Content)) -> Content {
    let mut c = level(Level::L0, T0, DAY);
    f(&mut c);
    c
}

/// Canonical files, byte for byte what the writer produces.
pub fn generated() -> Vec<(&'static str, Content)> {
    vec![
        ("l0-current.kvc", l0_current()),
        ("hour.kvc", level(Level::Hour, T0, DAY)),
        ("day.kvc", level(Level::Day, T0, DAY)),
        ("week.kvc", level(Level::Week, week_start(), 7 * DAY)),
        (
            "sparse.kvc",
            level(Level::Sparse, T0.div_euclid(91 * DAY) * 91 * DAY, 91 * DAY),
        ),
        ("revision.kvc", revision()),
        ("entries-two-generations.kvc", entries_two_generations()),
        ("entries-only.kvc", entries_only()),
        ("recovery-entry.kvc", recovery_entry()),
        ("set-aside.kvc", set_aside()),
        ("decimals.kvc", decimals()),
        ("nan.kvc", nan()),
        ("text-and-orphan.kvc", text_and_orphan()),
        ("state-bool.kvc", state_bool()),
        ("interval-reach.kvc", interval_reach()),
        ("long-series.kvc", long_series()),
        ("many-chunks.kvc", many_chunks()),
        ("many-batches.kvc", many_batches()),
        ("split-part.kvc", split_part()),
        ("week-l0.kvc", week_l0()),
    ]
}

/// Files with an element a v1 writer never writes.
pub fn hand_built() -> Vec<(&'static str, Content)> {
    vec![
        (
            "longer-trailer.kvc",
            tweaked(|c| c.tweaks.trailer_tail = vec![0xAB; 8]),
        ),
        (
            "unknown-ignorable.kvc",
            tweaked(|c| {
                c.tweaks.description_field = Some((0x8001, vec![1, 2, 3]));
                c.tweaks.times_column = Some((0x90, vec![5; 4]));
                c.tweaks.data_column = Some((0x90, vec![6; 3]));
            }),
        ),
        (
            "unknown-critical-data.kvc",
            tweaked(|c| c.tweaks.data_column = Some((0x12, vec![1]))),
        ),
        (
            "unknown-critical-times.kvc",
            tweaked(|c| c.tweaks.times_column = Some((0x05, vec![1]))),
        ),
        (
            "unknown-critical-description.kvc",
            tweaked(|c| c.tweaks.description_field = Some((14, vec![]))),
        ),
        (
            "undefined-flag.kvc",
            tweaked(|c| c.tweaks.chunk_flags = 1 << 4),
        ),
    ]
}
