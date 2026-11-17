//! Series definitions (`format.md` A.6, entry type 1).

use super::decode::Parsed;
use super::*;
use crate::bytes::u32_at;

pub(super) fn definition(p: &Parsed<'_>) -> Result<Definition, BodyError> {
    let kind = match p.u8_in(3, 5)? {
        1 => Kind::Gauge,
        2 => Kind::Counter,
        3 => Kind::State,
        4 => Kind::Text,
        5 => Kind::Interval,
        _ => return Err(p.bad(3)),
    };
    let value_type = ValueType::from_code(p.u8_in(4, 5)?).ok_or(p.bad(4))?;
    let decimal = value_type == ValueType::Decimal;
    let scale = if decimal {
        Some(p.u8_in(5, MAX_SCALE)?)
    } else {
        p.none(5).map(|_| None)?
    };
    let counter = if kind == Kind::Counter {
        Some(CounterRules {
            wrap_at: p.opt_u64_of(10, p.need(10)?)?,
            reset_ceiling_ppm: p.u32(11)?,
        })
    } else {
        p.none(10)?;
        p.none(11)?;
        None
    };
    // Distinct, so a state's index and its name count the same transitions (Q-182). Past the limit the verdict is the
    // same either way, so the names are not looked at.
    let names = p.with(13);
    if names.len() > MAX_STATE_NAMES || !p.names.borrow_mut().valid(names) {
        return Err(p.bad(13));
    }
    let carried = !names.is_empty();
    if (value_type == ValueType::State) != carried {
        return Err(p.presence(13));
    }
    let interval = if kind == Kind::Interval {
        Some(match p.u8_in(16, 2)? {
            0 => {
                let (length, phase) = (p.i64(17)?, p.i64(18)?);
                if length <= 0 {
                    return Err(p.bad(17));
                }
                if !(0..length).contains(&phase) {
                    return Err(p.bad(18));
                }
                p.none(19)?;
                p.none(22)?;
                IntervalLength::Fixed { length, phase }
            }
            tag => {
                p.none(17)?;
                p.none(18)?;
                let zone = p.own(p.need_text(19, 1, 63)?);
                if tag == 1 {
                    let day_start = p.i64(22)?;
                    if !(0..DAY).contains(&day_start) {
                        return Err(p.bad(22));
                    }
                    IntervalLength::Day { zone, day_start }
                } else {
                    p.none(22)?;
                    IntervalLength::Month { zone }
                }
            }
        })
    } else {
        for id in [16, 17, 18, 19, 22] {
            p.none(id)?;
        }
        None
    };
    let retention = match p.u8_in(23, 1)? {
        0 => p.none(24).map(|_| Retention::LongTerm)?,
        _ => Retention::Diagnostic { period: p.i64(24)? },
    };
    let excess_precision = if decimal {
        Some(match p.u8_in(25, 2)? {
            0 => p.none(26).map(|_| ExcessPrecision::Round)?,
            1 => p.none(26).map(|_| ExcessPrecision::Refuse)?,
            _ => {
                let max_scale = p.u8_in(26, MAX_SCALE)?;
                if Some(max_scale) < scale {
                    return Err(p.bad(26));
                }
                ExcessPrecision::AutoRaise { max_scale }
            }
        })
    } else {
        p.none(25)?;
        p.none(26)?;
        None
    };
    Ok(Definition {
        id: p.u32(1)?,
        name: p.own(p.need_text(2, 1, 255)?),
        kind,
        value_type,
        scale,
        unit: p.own(p.need_text(6, 0, 63)?),
        expected_interval: p.opt_i64_of(7, p.need(7)?, true)?,
        staleness: p.opt_i64_of(8, p.need(8)?, true)?,
        record_on_change: p.u8_in(9, 1)? == 1,
        counter,
        lateness_bound: p.i64(12)?,
        state_names: if p.keep {
            names
                .iter()
                .map(|f| String::from_utf8_lossy(f.1).into_owned())
                .collect()
        } else {
            Vec::new()
        },
        versioning: match p.u8_in(14, 2)? {
            0 => Versioning::None,
            1 => Versioning::Time,
            _ => Versioning::Version,
        },
        timing: match p.u8_in(15, 2)? {
            0 => Timing::Observed,
            1 => Timing::Scheduled,
            _ => Timing::Forecast,
        },
        interval,
        retention,
        excess_precision,
        staleness_set: p.u8_in(27, 1)? == 1,
    })
}

/// A state name: 1 to 63 bytes of UTF-8 (A.6 field 13).
fn name_ok(v: &[u8]) -> bool {
    (1..=63).contains(&v.len()) && (v.is_ascii() || std::str::from_utf8(v).is_ok())
}

/// Checks a definition's state names and tells them distinct in time linear in their count (Q-651, Q-870),
/// reusing its memory across the definitions of a body.
#[derive(Default)]
pub(super) struct Names {
    keys: Vec<u64>,
    /// Open addressing over keys; 0 is an empty slot, and no key is 0.
    table: Vec<u64>,
    random: Option<Random>,
}

/// Per-process random keys, so that no input is slow on purpose, only by chance.
struct Random {
    seed: u64,
    mul: u64,
    long: std::hash::RandomState,
}

/// Up to this many names are compared pairwise.
const PAIRWISE: usize = 8;
/// A probe run longer than this ends the table's attempt; the names are then sorted, so the worst case stays
/// `n log n` whatever the keys.
const RUN: usize = 128;
/// The top byte of a hashed key; a packed key's top byte is its length, at most 7.
const HASHED: u64 = 0xFF << 56;

impl Names {
    /// Whether the names, the values of `fields`, are each 1 to 63 bytes of UTF-8 (A.6 field 13) and distinct.
    pub(super) fn valid(&mut self, fields: &[(u16, &[u8])]) -> bool {
        self.valid_with(fields, RUN)
    }

    /// [`Names::valid`], sorting once a probe run exceeds `run_limit`.
    fn valid_with(&mut self, fields: &[(u16, &[u8])], run_limit: usize) -> bool {
        let n = fields.len();
        if n <= PAIRWISE {
            return fields.iter().all(|f| name_ok(f.1))
                && (1..n).all(|i| (0..i).all(|j| fields[i].1 != fields[j].1));
        }
        let random = self.random.get_or_insert_with(|| {
            use std::hash::{BuildHasher, Hasher};
            let long = std::hash::RandomState::new();
            let mut h = long.build_hasher();
            let seed = h.finish();
            h.write_u64(seed);
            Random {
                seed,
                mul: h.finish() | 1,
                long,
            }
        });
        self.keys.clear();
        for f in fields {
            match key(f.1, &random.long) {
                Some(k) => self.keys.push(k),
                None => return false,
            }
        }
        let bits = (2 * n).next_power_of_two().trailing_zeros();
        let mask = (1usize << bits) - 1;
        self.table.clear();
        self.table.resize(mask + 1, 0);
        for &key in &self.keys {
            let mut i = (spread(key, random) >> (64 - bits)) as usize;
            let mut run = 0;
            loop {
                let slot = &mut self.table[i];
                if *slot == 0 {
                    *slot = key;
                    break;
                }
                if *slot == key {
                    // Equal packed keys are equal names; equal hashed keys are decided by the sort.
                    if key & HASHED != HASHED {
                        return false;
                    }
                    return sorted_distinct(fields);
                }
                run += 1;
                if run > run_limit {
                    return sorted_distinct(fields);
                }
                i = (i + 1) & mask;
            }
        }
        true
    }
}

/// A keyed mix of a key whose top bits pick its home slot.
fn spread(key: u64, r: &Random) -> u64 {
    let mut x = key ^ r.seed;
    x = (x ^ (x >> 32)).wrapping_mul(r.mul);
    x = (x ^ (x >> 29)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^ (x >> 32)
}

fn sorted_distinct(fields: &[(u16, &[u8])]) -> bool {
    let mut sorted: Vec<&[u8]> = fields.iter().map(|f| f.1).collect();
    sorted.sort_unstable();
    sorted.windows(2).all(|w| w[0] != w[1])
}

/// A valid name as a key: one of 1 to 7 bytes as one word, its length in the top byte, so equal keys mean equal
/// names; a longer one by a keyed hash with `HASHED` as its top byte. `None` for an invalid name.
fn key(v: &[u8], long: &std::hash::RandomState) -> Option<u64> {
    use std::hash::BuildHasher;
    let n = v.len();
    let w = match n {
        1..=3 => {
            u64::from(v[0])
                | u64::from(v[n / 2]) << (8 * (n / 2))
                | u64::from(v[n - 1]) << (8 * (n - 1))
        }
        4..=7 => u64::from(u32_at(v, 0)) | u64::from(u32_at(v, n - 4)) << (8 * (n - 4)),
        _ => return name_ok(v).then(|| long.hash_one(v) | HASHED),
    };
    (w & 0x0080_8080_8080_8080 == 0 || name_ok(v)).then_some(w | (n as u64) << 56)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(names: &[Vec<u8>]) -> Vec<(u16, &[u8])> {
        names.iter().map(|n| (13, n.as_slice())).collect()
    }

    /// A packed key holds a name's length and every byte, so names that differ only there are told apart.
    #[test]
    fn packed_keys_tell_apart_names_that_differ_in_length_or_any_one_byte() {
        let names: Vec<Vec<u8>> = [
            &b"a"[..],
            b"a\0",
            b"a\0\0",
            b"\0a",
            b"aa",
            b"aaa",
            b"aab",
            b"aba",
            b"baa",
            b"abcd",
            b"abce",
            b"abdd",
            b"abcdefg",
            b"abcdefh",
            b"abcdeff",
            b"bbcdefg",
            b"abcdefgh",
        ]
        .iter()
        .map(|n| n.to_vec())
        .collect();
        assert!(Names::default().valid(&fields(&names)));
        for i in 1..names.len() {
            let mut repeated = names.clone();
            repeated[i - 1] = names[i].clone();
            assert!(!Names::default().valid(&fields(&repeated)), "{i}");
        }
    }

    /// The sort that ends a long probe run, and the one that settles equal hashed keys, give the table's verdict.
    #[test]
    fn the_sort_after_a_long_probe_run_and_after_equal_hashed_keys_gives_the_same_verdict() {
        for len in [3, 12] {
            // Enough names that some share a home slot, so a run limit of 0 always ends in the sort.
            let mut names: Vec<Vec<u8>> = (0..200u8)
                .map(|k| [b'a' + k % 26, b'a' + k / 26].repeat(len / 2 + 1))
                .collect();
            let all = fields(&names);
            for run in [0, RUN] {
                assert!(Names::default().valid_with(&all, run), "{len} {run}");
            }
            names[199] = names[3].clone();
            let repeated = fields(&names);
            for run in [0, RUN] {
                assert!(!Names::default().valid_with(&repeated, run), "{len} {run}");
            }
        }
    }
}
