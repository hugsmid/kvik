//! Exact numbers of node fields (Q-180): rationals in memory; a stored node holds dyadic ones only.

use num_bigint::{BigInt, BigUint, Sign};
use num_traits::Zero;

/// An exact rational: sums, integrals and increases of a node.
pub type Exact = num_rational::BigRational;

/// A finite `f64` as an exact rational; every finite `f64` is dyadic, so this is lossless.
///
/// # Panics
/// Panics on NaN or infinity, which never reach a summary (spec §2.5).
pub fn exact(v: f64) -> Exact {
    Exact::from_float(v).expect("finite value")
}

/// A whole number of nanoseconds as an exact rational.
pub fn exact_ns(ns: i128) -> Exact {
    Exact::from_integer(BigInt::from(ns))
}

/// A non-zero dyadic number ± `mantissa` × 2^`exp` with an odd mantissa: the stored form (`format-tree.md` T.3).
pub(crate) struct Dyadic {
    pub negative: bool,
    pub mantissa: BigUint,
    pub exp: i64,
}

/// The stored form of `x`; `None` for zero; `Err` when the denominator is not a power of two.
pub(crate) fn dyadic(x: &Exact) -> Result<Option<Dyadic>, ()> {
    if x.is_zero() {
        return Ok(None);
    }
    let d = x.denom().magnitude();
    if d.count_ones() != 1 {
        return Err(());
    }
    let d_log = i64::try_from(d.trailing_zeros().unwrap_or(0)).map_err(|_| ())?;
    let n = x.numer().magnitude();
    let tz = n.trailing_zeros().unwrap_or(0);
    Ok(Some(Dyadic {
        negative: x.numer().sign() == Sign::Minus,
        mantissa: n >> tz,
        exp: i64::try_from(tz).map_err(|_| ())? - d_log,
    }))
}

/// The rational ± `mantissa` × 2^`exp`.
pub(crate) fn from_dyadic(d: Dyadic) -> Exact {
    let sign = if d.negative { Sign::Minus } else { Sign::Plus };
    let m = BigInt::from_biguint(sign, d.mantissa);
    let shift = usize::try_from(d.exp.unsigned_abs()).expect("bounded exponent");
    let pow = BigInt::from(1) << shift;
    if d.exp >= 0 {
        Exact::from_integer(m * pow)
    } else {
        Exact::new(m, pow)
    }
}
