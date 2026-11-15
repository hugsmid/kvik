//! Exact numbers of node fields (Q-180): a dyadic mantissa and binary exponent, which sums of `f64` values and their
//! products with whole nanoseconds never leave, so folding and merging run no gcd; a rational only after a division
//! (the pro rata credit of a window, never stored).

use std::ops::{Add, AddAssign, Div, Mul, Neg, Sub};

use num_bigint::{BigInt, BigUint, Sign};
use num_rational::BigRational;
use num_traits::{One, Signed, Zero};

/// An exact number: a sum, integral or increase of a node.
#[derive(Clone, Debug)]
pub struct Exact(Repr);

#[derive(Clone, Debug)]
enum Repr {
    /// `m` × 2^`e`; `m` need not be odd.
    Dyadic(BigInt, i64),
    /// A rational whose denominator is not a power of two.
    Ratio(BigRational),
}

/// A finite `f64` as an exact number; every finite `f64` is dyadic, so this is lossless.
///
/// # Panics
/// Panics on NaN or infinity, which never reach a summary (spec §2.5).
pub fn exact(v: f64) -> Exact {
    assert!(v.is_finite(), "finite value");
    let bits = v.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i64;
    let frac = bits & ((1 << 52) - 1);
    let (m, e) = if biased == 0 {
        (frac, -1074)
    } else {
        (frac | (1 << 52), biased - 1075)
    };
    if m == 0 {
        return Exact::zero();
    }
    let tz = m.trailing_zeros();
    let m = BigInt::from(m >> tz);
    Exact(Repr::Dyadic(
        if bits >> 63 == 1 { -m } else { m },
        e + i64::from(tz),
    ))
}

/// A whole number of nanoseconds as an exact number.
pub fn exact_ns(ns: i128) -> Exact {
    if ns == 0 {
        return Exact::zero();
    }
    let tz = ns.trailing_zeros();
    Exact(Repr::Dyadic(BigInt::from(ns >> tz), i64::from(tz)))
}

/// A non-zero dyadic number ± `mantissa` × 2^`exp` with an odd mantissa: the stored form (`format-tree.md` T.3).
pub(crate) struct Dyadic {
    pub negative: bool,
    pub mantissa: BigUint,
    pub exp: i64,
}

impl Exact {
    /// Zero.
    pub fn zero() -> Exact {
        Exact(Repr::Dyadic(BigInt::zero(), 0))
    }

    /// Whether the number is zero.
    pub fn is_zero(&self) -> bool {
        match &self.0 {
            Repr::Dyadic(m, _) => m.is_zero(),
            Repr::Ratio(r) => r.is_zero(),
        }
    }

    /// The number as a rational.
    pub fn to_ratio(&self) -> BigRational {
        match &self.0 {
            Repr::Dyadic(m, e) => {
                let pow =
                    BigInt::one() << usize::try_from(e.unsigned_abs()).expect("bounded exponent");
                if *e >= 0 {
                    BigRational::from_integer(m * pow)
                } else {
                    BigRational::new(m.clone(), pow)
                }
            }
            Repr::Ratio(r) => r.clone(),
        }
    }

    /// The stored form: `None` for zero, `Err` for a number that is not dyadic.
    pub(crate) fn dyadic(&self) -> Result<Option<Dyadic>, ()> {
        let Repr::Dyadic(m, e) = &self.0 else {
            return Err(());
        };
        if m.is_zero() {
            return Ok(None);
        }
        let tz = m.trailing_zeros().unwrap_or(0);
        Ok(Some(Dyadic {
            negative: m.is_negative(),
            mantissa: m.magnitude() >> tz,
            exp: e + i64::try_from(tz).map_err(|_| ())?,
        }))
    }

    pub(crate) fn from_dyadic(d: Dyadic) -> Exact {
        let sign = if d.negative { Sign::Minus } else { Sign::Plus };
        Exact(Repr::Dyadic(BigInt::from_biguint(sign, d.mantissa), d.exp))
    }

    fn from_ratio(r: BigRational) -> Exact {
        let d = r.denom();
        if d.magnitude().count_ones() == 1 {
            let shift = d.magnitude().trailing_zeros().unwrap_or(0);
            let e = -i64::try_from(shift).expect("bounded exponent");
            return Exact(Repr::Dyadic(r.numer().clone(), e));
        }
        Exact(Repr::Ratio(r))
    }
}

impl PartialEq for Exact {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Repr::Dyadic(..), Repr::Dyadic(..)) => {
                let parts = |x: &Exact| {
                    x.dyadic()
                        .ok()
                        .flatten()
                        .map(|d| (d.negative, d.mantissa, d.exp))
                };
                parts(self) == parts(other)
            }
            (Repr::Ratio(a), Repr::Ratio(b)) => a == b,
            _ => false,
        }
    }
}

fn shifted(m: &BigInt, by: i64) -> BigInt {
    if by == 0 {
        m.clone()
    } else {
        m << usize::try_from(by).expect("bounded exponent")
    }
}

impl AddAssign<&Exact> for Exact {
    fn add_assign(&mut self, rhs: &Exact) {
        match (&mut self.0, &rhs.0) {
            (Repr::Dyadic(m, e), Repr::Dyadic(rm, re)) => {
                if rm.is_zero() {
                } else if m.is_zero() {
                    *m = rm.clone();
                    *e = *re;
                } else if *re >= *e {
                    *m += shifted(rm, *re - *e);
                } else {
                    *m = shifted(m, *e - *re) + rm;
                    *e = *re;
                }
            }
            _ => *self = Exact::from_ratio(self.to_ratio() + rhs.to_ratio()),
        }
    }
}

impl AddAssign for Exact {
    fn add_assign(&mut self, rhs: Exact) {
        *self += &rhs;
    }
}

impl Add<&Exact> for &Exact {
    type Output = Exact;
    fn add(self, rhs: &Exact) -> Exact {
        let mut out = self.clone();
        out += rhs;
        out
    }
}

impl Add for Exact {
    type Output = Exact;
    fn add(mut self, rhs: Exact) -> Exact {
        self += &rhs;
        self
    }
}

impl Neg for Exact {
    type Output = Exact;
    fn neg(self) -> Exact {
        Exact(match self.0 {
            Repr::Dyadic(m, e) => Repr::Dyadic(-m, e),
            Repr::Ratio(r) => Repr::Ratio(-r),
        })
    }
}

impl Sub for Exact {
    type Output = Exact;
    fn sub(self, rhs: Exact) -> Exact {
        self + -rhs
    }
}

impl Mul for Exact {
    type Output = Exact;
    fn mul(self, rhs: Exact) -> Exact {
        match (self.0, rhs.0) {
            (Repr::Dyadic(a, ae), Repr::Dyadic(b, be)) => Exact(Repr::Dyadic(a * b, ae + be)),
            (a, b) => Exact::from_ratio(Exact(a).to_ratio() * Exact(b).to_ratio()),
        }
    }
}

impl Div for Exact {
    type Output = Exact;
    /// # Panics
    /// Panics on division by zero.
    fn div(self, rhs: Exact) -> Exact {
        Exact::from_ratio(self.to_ratio() / rhs.to_ratio())
    }
}
