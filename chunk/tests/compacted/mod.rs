//! Compacted segments from synthetic facts, generated or built by hand, and the oracles the file tests compare
//! the reader with.

#![allow(dead_code)]

use kvik_chunk::file::{
    Batch, Bounds, Class, Description, Fact, FileWriter, Identity, Input, Level, Position, Reach,
    Series, SeriesInfo, Tweaks, Written,
};
use kvik_chunk::{flag, value_type};
use kvik_log::entries::Entry;
use proptest::prelude::*;

pub const STORE: [u8; 16] = [7; 16];
pub const SECOND: i64 = 1_000_000_000;
pub const DAY: i64 = 86_400 * SECOND;
/// 2026-01-01T00:00:00Z.
pub const T0: i64 = 1_767_225_600_000_000_000;

/// Everything a file is written from.
#[derive(Clone, Debug)]
pub struct Content {
    pub id: Identity,
    pub created_at: i64,
    pub description: Description,
    pub batches: Vec<Batch>,
    /// In the order of B.1.
    pub series: Vec<Series>,
    pub entries: Vec<(Position, Entry)>,
    pub tweaks: Tweaks,
}

pub fn id(file_id: u64) -> Identity {
    Identity {
        store_id: STORE,
        generation: 1,
        file_id,
    }
}

pub fn description(class: Class) -> Description {
    Description {
        class,
        level: Level::L0,
        period_start: T0,
        period_len: DAY,
        run_id: 40,
        run_count: 2,
        inputs: vec![
            Input {
                kind: 1,
                file_id: 3,
            },
            Input {
                kind: 1,
                file_id: 4,
            },
        ],
        name_range: None,
        has_entries: false,
        has_set_aside: false,
    }
}

impl Content {
    pub fn new(class: Class) -> Self {
        Self {
            id: id(40),
            created_at: T0 + 5 * DAY,
            description: description(class),
            batches: Vec::new(),
            series: Vec::new(),
            entries: Vec::new(),
            tweaks: Tweaks::default(),
        }
    }

    /// Sets the description's flags from the content and puts the series in the order of B.1.
    pub fn settle(mut self) -> Self {
        self.description.has_entries = !self.entries.is_empty();
        self.description.has_set_aside = self.series.iter().any(|s| !s.set_aside.is_empty());
        self.series.sort_by(|a, b| {
            let key = |s: &Series| (s.name.is_none(), s.name.clone(), s.series_id);
            key(a).cmp(&key(b))
        });
        self
    }

    pub fn write(&self) -> (Vec<u8>, Written) {
        let mut w = FileWriter::new(
            Vec::new(),
            self.id,
            self.created_at,
            &self.description,
            self.batches.clone(),
        )
        .unwrap()
        .with_tweaks(self.tweaks.clone());
        for s in &self.series {
            w.series(s).unwrap();
        }
        w.entries(&self.entries).unwrap();
        w.finish().unwrap()
    }

    pub fn info(&self, series_id: u32) -> SeriesInfo {
        let s = self.series.iter().find(|s| s.series_id == series_id);
        SeriesInfo {
            name: s.and_then(|s| s.name.clone()),
            reach: s.map_or(Reach::Point, |s| s.reach),
        }
    }

    /// The batches the file holds a position of.
    pub fn used_batches(&self) -> Vec<Batch> {
        let held = self.positions();
        self.batches
            .iter()
            .filter(|b| held.iter().any(|p| b.holds(p.0, p.1)))
            .copied()
            .collect()
    }

    pub fn positions(&self) -> Vec<Position> {
        let mut p: Vec<Position> = self
            .series
            .iter()
            .flat_map(|s| s.facts.iter().chain(&s.set_aside))
            .map(|f| f.position)
            .chain(self.entries.iter().map(|e| e.0))
            .collect();
        p.sort_unstable();
        p
    }

    pub fn batch_of(&self, p: Position) -> Batch {
        *self.batches.iter().find(|b| b.holds(p.0, p.1)).unwrap()
    }
}

/// Batches of consecutive sequences in one generation from `first`, `size` positions each, a second apart.
pub fn batches(generation: u32, first: u64, count: usize, size: u64) -> Vec<Batch> {
    (0..count as u64)
        .map(|i| Batch {
            generation,
            first_sequence: first + i * size,
            last_sequence: first + i * size + size - 1,
            received_at: T0 + i as i64 * SECOND,
            commit_time: T0 + i as i64 * SECOND + 2_000_000,
        })
        .collect()
}

/// The text of a text row, with its length as the value word.
pub fn text(series_id: u32, position: Position, t: i64, s: &str) -> Fact {
    Fact {
        text: s.as_bytes().to_vec(),
        ..Fact::value(series_id, position, t, value_type::TEXT, s.len() as u64)
    }
}

pub fn decimal(series_id: u32, position: Position, t: i64, v: i64, scale: u8) -> Fact {
    Fact {
        scale,
        ..Fact::value(series_id, position, t, value_type::DECIMAL, v as u64)
    }
}

pub fn f64_fact(series_id: u32, position: Position, t: i64, v: f64) -> Fact {
    Fact::value(series_id, position, t, value_type::F64, v.to_bits())
}

/// The bounds of a chunk's facts, computed independently of the writer: `f64` by `total_cmp`, decimals in `i128`.
pub fn oracle_bounds(facts: &[Fact]) -> (Option<Bounds>, bool) {
    let valued: Vec<&Fact> = facts
        .iter()
        .filter(|f| f.flags & flag::NO_VALUE == 0)
        .collect();
    let Some(first) = valued.first() else {
        return (None, false);
    };
    let vt = first.value_type;
    if vt == value_type::TEXT || valued.iter().any(|f| f.value_type != vt) {
        return (None, false);
    }
    match vt {
        value_type::F64 => {
            let nan = valued.iter().any(|f| f64::from_bits(f.value).is_nan());
            let mut v: Vec<f64> = valued
                .iter()
                .map(|f| f64::from_bits(f.value))
                .filter(|x| !x.is_nan())
                .collect();
            v.sort_by(f64::total_cmp);
            let b = (!v.is_empty()).then(|| Bounds::F64 {
                min: v[0].to_bits(),
                max: v[v.len() - 1].to_bits(),
            });
            (b, nan)
        }
        value_type::DECIMAL => {
            let scale = valued.iter().map(|f| f.scale).max().unwrap();
            let v: Vec<i128> = valued
                .iter()
                .map(|f| i128::from(f.value as i64) * 10i128.pow(u32::from(scale - f.scale)))
                .collect();
            if v.iter()
                .any(|&x| x > i128::from(i64::MAX) || x < i128::from(i64::MIN))
            {
                return (None, false);
            }
            let b = Bounds::Decimal {
                scale,
                min: *v.iter().min().unwrap() as i64,
                max: *v.iter().max().unwrap() as i64,
            };
            (Some(b), false)
        }
        _ => {
            let min = valued.iter().map(|f| f.value).min().unwrap();
            let max = valued.iter().map(|f| f.value).max().unwrap();
            let b = if vt == value_type::STATE {
                Bounds::State { min, max }
            } else {
                Bounds::Bool { min, max }
            };
            (Some(b), false)
        }
    }
}

/// Random choices from which [`build`] makes valid content.
#[derive(Clone, Debug)]
pub struct Spec {
    pub revision: bool,
    pub base_generation: u32,
    /// Per batch: starts the next generation, size, gap before it, receive step, lag.
    pub batches: Vec<(bool, u8, u8, u16, u8)>,
    /// Per position, cycled: 0 absent, 1–5 a fact, 6 and 7 an entry (a fact in a revision file).
    pub slots: Vec<u8>,
    /// Per series: name code (0 none), reach code, value type (0 per fact).
    pub series: Vec<(u8, u8, u8)>,
    /// Per fact, cycled: word, scale, flags, quality, jitter, `changed_at`.
    pub values: Vec<(u64, u8, u8, u8, u16, u64)>,
    /// In a revision file, every n-th fact is set aside (0: none).
    pub set_aside_every: u8,
}

pub fn spec() -> impl Strategy<Value = Spec> {
    (
        any::<bool>(),
        0u32..3,
        prop::collection::vec(
            (
                prop::bool::weighted(0.05),
                1u8..=6,
                prop_oneof![4 => Just(0u8), 1 => 1u8..4],
                0u16..2_000,
                0u8..4,
            ),
            1..40,
        ),
        prop::collection::vec(0u8..8, 1..16),
        prop::collection::vec((0u8..6, 0u8..4, 0u8..=5), 1..6),
        prop::collection::vec(
            (
                prop_oneof![
                    3 => any::<u64>(),
                    1 => prop::sample::select(vec![
                        f64::NAN.to_bits(),
                        (-0.0f64).to_bits(),
                        0,
                        f64::INFINITY.to_bits(),
                        0xFFF8_0000_0000_0001,
                    ]),
                ],
                0u8..=18,
                prop_oneof![3 => Just(0u8), 1 => any::<u8>()],
                0u8..4,
                any::<u16>(),
                any::<u64>(),
            ),
            1..12,
        ),
        0u8..4,
    )
        .prop_map(
            |(revision, base_generation, batches, slots, series, values, set_aside_every)| Spec {
                revision,
                base_generation,
                batches,
                slots,
                series,
                values,
                set_aside_every,
            },
        )
}

fn value_of(vt: u8, word: u64, scale: u8, flags: u16, k: usize) -> (u64, Vec<u8>, u8) {
    if flags & flag::NO_VALUE != 0 {
        return (
            0,
            Vec::new(),
            if vt == value_type::DECIMAL { scale } else { 0 },
        );
    }
    match vt {
        value_type::F64 => (word, Vec::new(), 0),
        value_type::DECIMAL => {
            let v = if word.is_multiple_of(7) {
                (word as i64).max(i64::MIN + 1)
            } else {
                (word % 2_000_001) as i64 - 1_000_000
            };
            (v as u64, Vec::new(), scale)
        }
        value_type::STATE => (word % 65_536, Vec::new(), 0),
        value_type::BOOL => (word % 2, Vec::new(), 0),
        _ => {
            let s = format!("v{}é{}", word % 1000, k % 3);
            (s.len() as u64, s.into_bytes(), 0)
        }
    }
}

/// Valid content from a [`Spec`], and the positions of its batches that it does not hold.
pub fn build(s: &Spec) -> (Content, Vec<Position>) {
    let class = if s.revision {
        Class::Revision
    } else {
        Class::Current
    };
    let mut c = Content::new(class);
    let mut generation = s.base_generation;
    let mut next = 1u64;
    let mut received = T0;
    let mut commit = T0;
    let mut generations = 1;
    for &(new_gen, size, gap, step, lag) in &s.batches {
        if new_gen && generations < 2 && !c.batches.is_empty() {
            generation += 1;
            generations += 1;
            next = 1;
        }
        next += u64::from(gap);
        received = (received + i64::from(step)).max(commit);
        commit = received + i64::from(lag);
        let last = next + u64::from(size) - 1;
        c.batches.push(Batch {
            generation,
            first_sequence: next,
            last_sequence: last,
            received_at: received,
            commit_time: commit,
        });
        next = last + 1;
    }
    let n_series = s.series.len();
    let mut series: Vec<Series> = s
        .series
        .iter()
        .enumerate()
        .map(|(i, &(name, reach, _))| Series {
            series_id: 10 + 3 * i as u32,
            name: (name != 0).then(|| format!("site/{}/s{i}", name)),
            reach: match reach {
                0 => Reach::Point,
                1 => Reach::Fixed(900 * SECOND),
                2 => Reach::Day,
                _ => Reach::Month,
            },
            facts: Vec::new(),
            set_aside: Vec::new(),
        })
        .collect();
    let mut absent = Vec::new();
    let mut k = 0usize;
    let mut facts = 0usize;
    let mut counters = vec![0i64; n_series];
    let positions: Vec<Position> = c
        .batches
        .iter()
        .flat_map(|b| (b.first_sequence..=b.last_sequence).map(move |q| (b.generation, q)))
        .collect();
    let mut any = false;
    for (n, &p) in positions.iter().enumerate() {
        let mut slot = s.slots[k % s.slots.len()];
        k += 1;
        if n == positions.len() - 1 && !any {
            slot = 1;
        }
        match slot {
            0 => absent.push(p),
            6 | 7 if !s.revision => {
                any = true;
                let id = series[(k + n) % n_series].series_id;
                let entry = match (k + n) % 3 {
                    0 => Entry::Alias {
                        id,
                        name: format!("alias/{n}"),
                    },
                    1 => Entry::Retirement {
                        id,
                        t_r: T0 + n as i64,
                    },
                    _ => Entry::ScaleRaise { id, scale: 6 },
                };
                c.entries.push((p, entry));
            }
            _ => {
                any = true;
                let si = (usize::from(slot) + n) % n_series;
                let (word, scale, flags, quality, jitter, changed) =
                    s.values[facts % s.values.len()];
                facts += 1;
                let vt = match s.series[si].2 {
                    0 => (word % 5) as u8 + 1,
                    v => v,
                };
                let flags = u16::from(flags);
                let (value, text, scale) = value_of(vt, word, scale, flags, facts);
                let kk = counters[si];
                counters[si] += 1;
                let t = if s.revision {
                    T0 + (kk / 2) * SECOND
                } else {
                    T0 + kk * SECOND + i64::from(jitter)
                };
                let f = Fact {
                    series_id: series[si].series_id,
                    position: p,
                    t,
                    quality,
                    flags,
                    value_type: vt,
                    value,
                    text,
                    changed_at: if flags & flag::HAS_CHANGED_AT != 0 {
                        changed
                    } else {
                        0
                    },
                    scale,
                };
                if s.revision
                    && s.set_aside_every > 0
                    && facts.is_multiple_of(usize::from(s.set_aside_every))
                {
                    series[si].set_aside.push(f);
                } else {
                    series[si].facts.push(f);
                }
            }
        }
    }
    c.series = series
        .into_iter()
        .filter(|s| !s.facts.is_empty() || !s.set_aside.is_empty())
        .collect();
    (c.settle(), absent)
}

/// Writes `c`, then checks everything the reader gives back against `c` and the oracles: counts and ranges, every
/// chunk's statistics and facts with their batch's times, the entries, the batch rows, the layout, the canonical
/// checks, writing again, and the chunk index object. Returns the file.
pub fn check_round_trip(c: &Content) -> Vec<u8> {
    use kvik_chunk::file::{CompactedFile, IndexObject, check_canonical, extract};
    let (bytes, written) = c.write();
    assert_eq!(written.len as usize, bytes.len());
    let file = CompactedFile::open(bytes.as_slice(), &c.id, Some(&written.layout)).unwrap();
    assert_eq!(file.layout(), written.layout);
    let t = *file.trailer();
    let positions = c.positions();
    let all: Vec<&Fact> = c
        .series
        .iter()
        .flat_map(|s| s.facts.iter().chain(&s.set_aside))
        .collect();
    assert_eq!(t.entry_count as usize, positions.len());
    assert_eq!(t.fact_count as usize, all.len());
    assert_eq!(
        (t.first, t.last),
        (positions[0], positions[positions.len() - 1])
    );
    let used = c.used_batches();
    assert_eq!(
        t.first_commit_time,
        used.iter().map(|b| b.commit_time).min().unwrap()
    );
    assert_eq!(
        t.last_commit_time,
        used.iter().map(|b| b.commit_time).max().unwrap()
    );
    let mut chunks = 0;
    for s in &c.series {
        let mut got = (Vec::new(), Vec::new());
        for e in file.series(s.series_id).unwrap() {
            chunks += 1;
            let rows = file.read_chunk(&e).unwrap();
            let facts: Vec<Fact> = rows.iter().map(|r| r.fact.clone()).collect();
            assert_eq!(e.row_count as usize, facts.len());
            assert_eq!(e.min_t, facts.iter().map(|f| f.t).min().unwrap());
            assert_eq!(e.max_t, facts.iter().map(|f| f.t).max().unwrap());
            assert_eq!(
                e.reach,
                facts.iter().map(|f| s.reach.of(f.t)).max().unwrap()
            );
            assert_eq!(e.first, facts.iter().map(|f| f.position).min().unwrap());
            assert_eq!(e.last, facts.iter().map(|f| f.position).max().unwrap());
            assert_eq!(
                (e.bounds(), e.has_nan()),
                oracle_bounds(&facts),
                "bounds of {e:?}"
            );
            for r in &rows {
                let b = c.batch_of(r.fact.position);
                assert_eq!(
                    (r.received_at, r.commit_time),
                    (b.received_at, b.commit_time)
                );
            }
            if e.set_aside() {
                got.1.extend(facts);
            } else {
                got.0.extend(facts);
            }
        }
        for (got, want) in [(&mut got.0, &s.facts), (&mut got.1, &s.set_aside)] {
            let mut want = want.clone();
            want.sort_by_key(|f| f.position);
            got.sort_by_key(|f| f.position);
            assert_eq!(*got, want, "facts of series {}", s.series_id);
        }
    }
    assert_eq!(chunks, t.chunk_count as usize);
    assert_eq!(file.index().unwrap().len(), chunks);
    let entries = file.entries().unwrap();
    let back: Vec<(Position, Entry)> = entries
        .iter()
        .map(|e| (e.position, e.entry.clone()))
        .collect();
    assert_eq!(back, c.entries);
    for e in &entries {
        let b = c.batch_of(e.position);
        assert_eq!(
            (e.received_at, e.commit_time),
            (b.received_at, b.commit_time)
        );
    }
    assert_eq!(file.batches().unwrap(), used);
    check_canonical(&bytes, &c.id, &|id| c.info(id)).unwrap();
    assert_eq!(c.write().0, bytes, "writing again gives the same bytes");
    let object = extract(&file, [9; 32]).unwrap();
    let o = IndexObject::open(object.as_slice(), &c.id).unwrap();
    assert_eq!(o.layout(), &written.layout);
    assert_eq!(o.segment_trailer(), file.trailer());
    assert_eq!((o.segment_len(), o.sha256()), (bytes.len() as u64, [9; 32]));
    assert_eq!(o.times_directory(), file.times_directory());
    assert_eq!(o.index().unwrap(), file.index().unwrap());
    bytes
}

/// A pseudo-random `f64` bit pattern that no decimal or XOR encoding shortens, so value columns stay plain.
pub fn noise(i: u64, seed: u64) -> u64 {
    let mut x = (i + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seed;
    x ^= x >> 31;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    (x ^ (x >> 29)) & !(0x7FF << 52) | (0x3FF << 52)
}

/// A current file of two generations of 40 batches each: series 1 and 2 with the same shape (plain values from
/// `seed`), a decimal series 3, and an alias entry in each generation's first three batches.
pub fn sample(seed: u64) -> Content {
    let mut c = Content::new(Class::Current);
    c.batches = batches(1, 1, 40, 4);
    c.batches.extend(batches(2, 1, 40, 4));
    let mut s = [1u32, 2, 3].map(|id| Series {
        series_id: id,
        name: Some(format!("plant/{id}")),
        reach: Reach::Point,
        facts: Vec::new(),
        set_aside: Vec::new(),
    });
    for (n, b) in c.batches.clone().iter().enumerate() {
        let g = b.generation;
        let q = b.first_sequence;
        let t = T0 + n as i64 * SECOND;
        s[0].facts.push(f64_fact(
            1,
            (g, q),
            t,
            f64::from_bits(noise(n as u64, seed)),
        ));
        s[1].facts.push(f64_fact(
            2,
            (g, q + 1),
            t,
            f64::from_bits(noise(n as u64, 77)),
        ));
        s[2].facts.push(decimal(3, (g, q + 2), t, n as i64 * 3, 2));
        if n % 40 < 3 {
            c.entries.push((
                (g, q + 3),
                Entry::Alias {
                    id: 1,
                    name: format!("alias/{}", n % 10),
                },
            ));
        }
    }
    c.series = s.into_iter().collect();
    c.settle()
}

/// The blocks of a file in order, from the description to the trailer: offset, type and length with padding.
pub fn blocks(bytes: &[u8]) -> Vec<(usize, u8, usize)> {
    let end = bytes.len() - 16;
    let mut out = Vec::new();
    let mut at = 64;
    while at < end {
        let body_len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let len = 16 + body_len.div_ceil(8) * 8;
        out.push((at, bytes[at], len));
        at += len;
    }
    assert_eq!(at, end);
    out
}
