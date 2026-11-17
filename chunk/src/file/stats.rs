//! A chunk's pruning statistics (`docs/format-compacted.md` B.1 Bounds) and a reading's reach (`format.md` A.8).

use super::parts::RawBounds;
use crate::{flag, value_type};

/// How far a reading of a series reaches past its `t` (`format.md` A.8), from the series' kind and interval length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reach {
    /// Not an interval series: `t`.
    Point,
    /// A fixed interval of this many nanoseconds: `t + d`.
    Fixed(i64),
    /// A `day` interval: `t + 50 h`.
    Day,
    /// A `month` interval: `t + 33` days.
    Month,
}

const HOUR: i64 = 3_600_000_000_000;

impl Reach {
    /// The reach of a reading at `t`, saturating as the segment index's does.
    pub fn of(self, t: i64) -> i64 {
        match self {
            Self::Point => t,
            Self::Fixed(d) => t.saturating_add(d),
            Self::Day => t.saturating_add(50 * HOUR),
            Self::Month => t.saturating_add(33 * 24 * HOUR),
        }
    }
}

/// A chunk's value bounds, over every row that has a value word, whatever its quality.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bounds {
    /// Bit patterns of the least and greatest `f64` in IEEE 754 total order, NaN left out.
    F64 {
        /// Least.
        min: u64,
        /// Greatest.
        max: u64,
    },
    /// Decimals at the chunk's highest stored scale.
    Decimal {
        /// The scale both are at.
        scale: u8,
        /// Least.
        min: i64,
        /// Greatest.
        max: i64,
    },
    /// State indexes.
    State {
        /// Least.
        min: u64,
        /// Greatest.
        max: u64,
    },
    /// Bools, 0 or 1.
    Bool {
        /// Least.
        min: u64,
        /// Greatest.
        max: u64,
    },
}

impl Bounds {
    pub(crate) fn from_raw(r: RawBounds) -> Option<Self> {
        Some(match r.value_type {
            value_type::F64 => Self::F64 {
                min: r.min,
                max: r.max,
            },
            value_type::DECIMAL => Self::Decimal {
                scale: r.scale,
                min: r.min as i64,
                max: r.max as i64,
            },
            value_type::STATE => Self::State {
                min: r.min,
                max: r.max,
            },
            value_type::BOOL => Self::Bool {
                min: r.min,
                max: r.max,
            },
            _ => return None,
        })
    }

    pub(crate) fn raw(self) -> RawBounds {
        let (value_type, scale, min, max) = match self {
            Self::F64 { min, max } => (value_type::F64, 0, min, max),
            Self::Decimal { scale, min, max } => {
                (value_type::DECIMAL, scale, min as u64, max as u64)
            }
            Self::State { min, max } => (value_type::STATE, 0, min, max),
            Self::Bool { min, max } => (value_type::BOOL, 0, min, max),
        };
        RawBounds {
            value_type,
            scale,
            min,
            max,
        }
    }

    /// The `f64` bounds as numbers: a reader compares them numerically, so a predicate on zero matches either sign.
    pub fn f64_range(self) -> Option<(f64, f64)> {
        match self {
            Self::F64 { min, max } => Some((f64::from_bits(min), f64::from_bits(max))),
            _ => None,
        }
    }
}

/// The key of an `f64` bit pattern in IEEE 754 total order.
pub(crate) fn total_order(bits: u64) -> u64 {
    if bits >> 63 == 1 {
        !bits
    } else {
        bits | (1 << 63)
    }
}

/// The bounds of rows given as (flags, value type, value word, stored scale), and whether a value word is a NaN.
/// Bounds exist only when every row with a value word has one type and it is not text; `has_nan` only when that
/// type is `f64`. Decimals are brought exactly to the highest stored scale; a product outside `i64` gives no
/// bounds (Q-802: never saturated).
pub(crate) fn bounds(rows: impl Iterator<Item = (u16, u8, u64, u8)>) -> (Option<Bounds>, bool) {
    let mut vt: Option<u8> = None;
    let mut words: Vec<(u64, u8)> = Vec::new();
    for (flags, t, w, scale) in rows {
        if flags & flag::NO_VALUE != 0 {
            continue;
        }
        if vt.is_some_and(|v| v != t) || t == value_type::TEXT {
            return (None, false);
        }
        vt = Some(t);
        words.push((w, scale));
    }
    let Some(vt) = vt else {
        return (None, false);
    };
    match vt {
        value_type::F64 => {
            let has_nan = words.iter().any(|&(w, _)| f64::from_bits(w).is_nan());
            let numbers = words
                .iter()
                .map(|&(w, _)| w)
                .filter(|&w| !f64::from_bits(w).is_nan());
            let min = numbers.clone().min_by_key(|&w| total_order(w));
            let max = numbers.max_by_key(|&w| total_order(w));
            let b = min.zip(max).map(|(min, max)| Bounds::F64 { min, max });
            (b, has_nan)
        }
        value_type::DECIMAL => {
            let scale = words.iter().map(|&(_, s)| s).max().unwrap_or(0);
            let mut lo = i64::MAX;
            let mut hi = i64::MIN;
            for &(w, s) in &words {
                let Some(v) = scale
                    .checked_sub(s)
                    .and_then(|k| 10i64.checked_pow(u32::from(k)))
                    .and_then(|p| (w as i64).checked_mul(p))
                else {
                    return (None, false);
                };
                lo = lo.min(v);
                hi = hi.max(v);
            }
            (
                Some(Bounds::Decimal {
                    scale,
                    min: lo,
                    max: hi,
                }),
                false,
            )
        }
        _ => {
            let min = words.iter().map(|&(w, _)| w).min().unwrap_or(0);
            let max = words.iter().map(|&(w, _)| w).max().unwrap_or(0);
            let b = if vt == value_type::STATE {
                Bounds::State { min, max }
            } else {
                Bounds::Bool { min, max }
            };
            (Some(b), false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(v: f64) -> (u16, u8, u64, u8) {
        (0, value_type::F64, v.to_bits(), 0)
    }

    #[test]
    fn f64_bounds_use_total_order_without_nan() {
        let (b, nan) = bounds([f(1.0), f(-0.0), f(0.0), f(f64::NAN), f(-3.5)].into_iter());
        assert!(nan);
        assert_eq!(
            b,
            Some(Bounds::F64 {
                min: (-3.5f64).to_bits(),
                max: 1.0f64.to_bits()
            })
        );
        let (b, _) = bounds([f(0.0), f(-0.0)].into_iter());
        assert_eq!(
            b,
            Some(Bounds::F64 {
                min: (-0.0f64).to_bits(),
                max: 0.0f64.to_bits()
            })
        );
        let (b, nan) = bounds([f(f64::NAN), f(-f64::NAN)].into_iter());
        assert_eq!((b, nan), (None, true));
        let (b, _) = bounds([f(f64::NEG_INFINITY), f(f64::INFINITY)].into_iter());
        assert_eq!(
            b.and_then(Bounds::f64_range),
            Some((f64::NEG_INFINITY, f64::INFINITY))
        );
    }

    #[test]
    fn decimal_bounds_rise_to_the_highest_scale_and_never_saturate() {
        let d = |v: i64, s: u8| (0, value_type::DECIMAL, v as u64, s);
        let (b, _) = bounds([d(15, 1), d(-7, 3), d(2, 0)].into_iter());
        assert_eq!(
            b,
            Some(Bounds::Decimal {
                scale: 3,
                min: -7,
                max: 2000
            })
        );
        let (b, _) = bounds([d(i64::MAX / 5, 0), d(1, 1)].into_iter());
        assert_eq!(b, None);
    }

    #[test]
    fn rows_without_a_value_take_no_part_and_text_or_mixed_types_give_none() {
        let none = (flag::NO_VALUE, value_type::TEXT, 0, 0);
        let (b, _) = bounds([none, (0, value_type::STATE, 4, 0)].into_iter());
        assert_eq!(b, Some(Bounds::State { min: 4, max: 4 }));
        assert_eq!(
            bounds([(0, value_type::TEXT, 3, 0)].into_iter()),
            (None, false)
        );
        assert_eq!(
            bounds([f(f64::NAN), (0, value_type::BOOL, 1, 0)].into_iter()),
            (None, false)
        );
        assert_eq!(bounds([none].into_iter()), (None, false));
    }
}
