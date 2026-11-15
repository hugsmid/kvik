//! The canonical byte layout of a stored node (`docs/format-tree.md`): [`encode`] writes the one byte sequence for a
//! node; [`decode`] applies the checks of T.6, never panics and never allocates from an unchecked length.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use num_bigint::BigUint;
use num_traits::Zero;

use crate::exact::{Dyadic, Exact, dyadic, from_dyadic};
use crate::node::{Body, Kind, Node, StateKey, Value};

/// Form ids of a stored node (`format-tree.md` T.2); every other id is reserved, decimal forms among them.
pub mod form {
    /// Gauge.
    pub const GAUGE: u8 = 1;
    /// Counter.
    pub const COUNTER: u8 = 2;
    /// State.
    pub const STATE: u8 = 3;
    /// Text.
    pub const TEXT: u8 = 4;
    /// Interval.
    pub const INTERVAL: u8 = 5;
}

/// The lowest binary exponent of a stored exact number: that of the smallest subnormal `f64` (T.3).
const MIN_EXP: i64 = -1074;
/// A stored exact number's magnitude is below 2^`MAX_BITS` (T.3).
const MAX_BITS: i64 = 1100;
/// The longest stored mantissa in bytes, implied by [`MIN_EXP`] and [`MAX_BITS`].
const MAX_MANTISSA_BYTES: u64 = ((MAX_BITS - MIN_EXP) as u64).div_ceil(8);

/// A node that has no stored form (T.1): a window evaluation, or fields no fold and merge of readings give.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a storable node: {0}")]
pub struct NotStorable(pub &'static str);

/// Why bytes are not a stored node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The bytes end inside a field.
    #[error("node ends inside a field at byte {0}")]
    Truncated(usize),
    /// Bytes follow the node.
    #[error("{0} bytes after the node")]
    Trailing(usize),
    /// A form id this version does not know: a version refusal, never corruption (`format.md` common rules).
    #[error("unsupported node form {0:#04x}")]
    UnsupportedForm(u8),
    /// A field fails a check of T.6.
    #[error("invalid node at byte {at}: {what}")]
    Invalid {
        /// Offset of the field.
        at: usize,
        /// The check that failed.
        what: &'static str,
    },
}

/// The canonical bytes of a stored node (`format-tree.md` T.2–T.5).
///
/// # Errors
/// [`NotStorable`] for a node that fails a check of T.6, or that a fold and merge of readings cannot give and
/// the bytes would not show (a held `last`, a stretch step, an apportioned amount, a value of another kind, an exact
/// field that is not dyadic or out of range). A stretched window that keeps within its readings is not told apart (T.1).
pub fn encode(n: &Node) -> Result<Vec<u8>, NotStorable> {
    let kind = n.body.kind();
    let mut w = Vec::with_capacity(64);
    w.push(form_of(kind));
    put_i64(&mut w, n.start);
    let width = i128::from(n.end) - i128::from(n.start);
    put_uv(
        &mut w,
        u128::try_from(width).map_err(|_| NotStorable("negative span"))?,
    );
    put_uv(&mut w, u128::from(n.count));
    let coverage = u128::try_from(n.coverage).map_err(|_| NotStorable("negative coverage"))?;
    if n.count == 0 && coverage != 0 {
        return Err(NotStorable("coverage without readings"));
    }
    put_uv(&mut w, coverage);
    match (n.count, &n.first, &n.last) {
        (0, None, None) => {}
        (1, Some(f), Some(l)) if f == l => {
            check_inside(n, f.0)?;
            put_uv(&mut w, (i128::from(f.0) - i128::from(n.start)) as u128);
            put_value(&mut w, kind, &f.1)?;
        }
        (2.., Some(f), Some(l)) if f.0 < l.0 => {
            check_inside(n, f.0)?;
            check_inside(n, l.0)?;
            put_uv(&mut w, (i128::from(f.0) - i128::from(n.start)) as u128);
            put_value(&mut w, kind, &f.1)?;
            put_uv(&mut w, (i128::from(l.0) - i128::from(f.0)) as u128);
            put_value(&mut w, kind, &l.1)?;
        }
        _ => return Err(NotStorable("first and last do not match the count")),
    }
    let steps = n.count.saturating_sub(1);
    match &n.body {
        Body::Gauge {
            sum,
            min,
            max,
            integral,
        } => {
            put_exact(&mut w, sum)?;
            put_exact(&mut w, integral)?;
            put_extremes(&mut w, n.count, *min, *max)?;
        }
        Body::Counter {
            increase,
            resets,
            regressions,
            steps: s,
            long_steps,
            elapsed,
        } => {
            let span = match (&n.first, &n.last) {
                (Some(f), Some(l)) => i128::from(l.0) - i128::from(f.0),
                _ => 0,
            };
            if *s != steps || *elapsed != span {
                return Err(NotStorable("steps from outside the node"));
            }
            put_exact(&mut w, increase)?;
            put_uv(&mut w, u128::from(*resets));
            put_uv(&mut w, u128::from(*regressions));
            put_uv(&mut w, u128::from(*long_steps));
        }
        Body::State { time, transitions } => {
            put_uv(&mut w, u128::from(*transitions));
            put_uv(&mut w, time.len() as u128);
            for (k, ns) in time {
                put_key(&mut w, *k);
                put_uv(
                    &mut w,
                    u128::try_from(*ns)
                        .ok()
                        .filter(|ns| *ns > 0)
                        .ok_or(NotStorable("a state time that is not positive"))?,
                );
            }
        }
        Body::Text => {}
        Body::Interval {
            sum,
            min,
            max,
            apportioned,
        } => {
            if *apportioned {
                return Err(NotStorable("an apportioned amount"));
            }
            put_exact(&mut w, sum)?;
            put_extremes(&mut w, n.count, *min, *max)?;
        }
    }
    // Every node `decode` refuses is refused here too, so no writer output fails the reader's checks.
    decode(&w).map_err(|e| match e {
        DecodeError::Invalid { what, .. } => NotStorable(what),
        _ => NotStorable("bytes the reader refuses"),
    })?;
    Ok(w)
}

/// The node held by `bytes`, which must be exactly one stored node passing the checks of T.6.
///
/// # Errors
/// [`DecodeError`] naming the first failed check; an unknown form is [`DecodeError::UnsupportedForm`].
pub fn decode(bytes: &[u8]) -> Result<Node, DecodeError> {
    let mut r = Reader { b: bytes, at: 0 };
    let f = r.u8()?;
    let kind = match f {
        form::GAUGE => Kind::Gauge,
        form::COUNTER => Kind::Counter,
        form::STATE => Kind::State,
        form::TEXT => Kind::Text,
        form::INTERVAL => Kind::Interval,
        other => return Err(DecodeError::UnsupportedForm(other)),
    };
    let start = r.i64()?;
    let at = r.at;
    let width = r.uv64()?;
    let end = i64::try_from(i128::from(start) + i128::from(width))
        .map_err(|_| r.invalid(at, "span end beyond the time range"))?;
    let count = r.uv64()?;
    let at = r.at;
    let coverage = r.uv_i128()?;
    if count == 0 && coverage != 0 {
        return Err(r.invalid(at, "coverage without readings"));
    }
    let (first, last) = if count == 0 {
        (None, None)
    } else {
        let at = r.at;
        let dt = r.uv64()?;
        if dt >= width {
            return Err(r.invalid(at, "first reading outside the span"));
        }
        let ft = (i128::from(start) + i128::from(dt)) as i64;
        let fv = r.value(kind)?;
        let last = if count == 1 {
            (ft, fv.clone())
        } else {
            let at = r.at;
            let dt = r.uv64()?;
            if dt == 0 || i128::from(ft) + i128::from(dt) >= i128::from(end) {
                return Err(r.invalid(at, "last reading not after the first inside the span"));
            }
            ((i128::from(ft) + i128::from(dt)) as i64, r.value(kind)?)
        };
        (Some((ft, fv)), Some(last))
    };
    let held = match (&first, &last) {
        (Some(f), Some(l)) => i128::from(l.0) - i128::from(f.0),
        _ => 0,
    };
    // Holds lie between consecutive readings (spec §5.2), so a fold or merge never covers more than this.
    if kind != Kind::Interval && coverage > held {
        return Err(r.invalid(at, "coverage beyond the first and last readings"));
    }
    let steps = count.saturating_sub(1);
    let body = match kind {
        Kind::Gauge => {
            let at = r.at;
            let sum = r.exact()?;
            let integral = r.exact()?;
            if count == 0 && !sum.is_zero() {
                return Err(r.invalid(at, "sum without readings"));
            }
            if coverage == 0 && !integral.is_zero() {
                return Err(r.invalid(at, "integral without coverage"));
            }
            let (min, max) = r.extremes(count, &first, &last)?;
            Body::Gauge {
                sum,
                min,
                max,
                integral,
            }
        }
        Kind::Counter => {
            let at = r.at;
            let increase = r.exact()?;
            let resets = r.uv64()?;
            let regressions = r.uv64()?;
            let long_steps = r.uv64()?;
            if steps == 0 && !increase.is_zero() {
                return Err(r.invalid(at, "increase without a step"));
            }
            if resets.checked_add(regressions).is_none_or(|x| x > steps) || long_steps > steps {
                return Err(r.invalid(at, "more decreases or long steps than steps"));
            }
            Body::Counter {
                increase,
                resets,
                regressions,
                steps,
                long_steps,
                elapsed: held,
            }
        }
        Kind::State => {
            let at = r.at;
            let transitions = r.uv64()?;
            if transitions > steps {
                return Err(r.invalid(at, "more transitions than pairs"));
            }
            let at = r.at;
            let entries = r.uv64()?;
            // An entry takes at least two bytes.
            if entries > (r.b.len() - r.at) as u64 / 2 {
                return Err(r.invalid(at, "more states than bytes"));
            }
            let mut time = BTreeMap::new();
            let mut total: i128 = 0;
            let mut prev: Option<StateKey> = None;
            for _ in 0..entries {
                let at = r.at;
                let k = r.key()?;
                if prev.is_some_and(|p| p >= k) {
                    return Err(r.invalid(at, "states not in ascending order"));
                }
                let at = r.at;
                let ns = r.uv_i128()?;
                if ns == 0 {
                    return Err(r.invalid(at, "a state time of zero"));
                }
                total = total
                    .checked_add(ns)
                    .ok_or_else(|| r.invalid(at, "state times overflow"))?;
                time.insert(k, ns);
                prev = Some(k);
            }
            if total != coverage {
                return Err(r.invalid(at, "state times do not add up to the coverage"));
            }
            Body::State { time, transitions }
        }
        Kind::Text => Body::Text,
        Kind::Interval => {
            let at = r.at;
            let sum = r.exact()?;
            if count == 0 && !sum.is_zero() {
                return Err(r.invalid(at, "sum without readings"));
            }
            let (min, max) = r.extremes(count, &first, &last)?;
            Body::Interval {
                sum,
                min,
                max,
                apportioned: false,
            }
        }
    };
    if r.at != bytes.len() {
        return Err(DecodeError::Trailing(bytes.len() - r.at));
    }
    Ok(Node {
        start,
        end,
        count,
        first,
        last,
        coverage,
        body,
    })
}

fn form_of(kind: Kind) -> u8 {
    match kind {
        Kind::Gauge => form::GAUGE,
        Kind::Counter => form::COUNTER,
        Kind::State => form::STATE,
        Kind::Text => form::TEXT,
        Kind::Interval => form::INTERVAL,
    }
}

fn check_inside(n: &Node, t: i64) -> Result<(), NotStorable> {
    if n.start <= t && t < n.end {
        Ok(())
    } else {
        Err(NotStorable("a reading outside the span"))
    }
}

fn put_i64(w: &mut Vec<u8>, v: i64) {
    w.extend_from_slice(&v.to_le_bytes());
}

/// Minimal unsigned LEB128 (`format-chunk.md` C.2).
fn put_uv(w: &mut Vec<u8>, mut v: u128) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            w.push(byte);
            return;
        }
        w.push(byte | 0x80);
    }
}

fn put_f64(w: &mut Vec<u8>, v: f64) -> Result<(), NotStorable> {
    if !v.is_finite() {
        return Err(NotStorable("a value that is not finite"));
    }
    w.extend_from_slice(&v.to_bits().to_le_bytes());
    Ok(())
}

fn put_key(w: &mut Vec<u8>, k: StateKey) {
    match k {
        StateKey::Bool(b) => w.push(u8::from(b)),
        StateKey::State(i) => {
            w.push(2);
            w.extend_from_slice(&i.to_le_bytes());
        }
    }
}

fn put_value(w: &mut Vec<u8>, kind: Kind, v: &Value) -> Result<(), NotStorable> {
    match (kind, v) {
        (Kind::Gauge | Kind::Counter | Kind::Interval, Value::F64(x)) => put_f64(w, *x),
        (Kind::State, Value::Bool(_) | Value::State(_)) => {
            put_key(w, v.state_key().expect("state value"));
            Ok(())
        }
        (Kind::Text, Value::Text(s)) => {
            let len = u8::try_from(s.len()).map_err(|_| NotStorable("text over 255 bytes"))?;
            w.push(len);
            w.extend_from_slice(s.as_bytes());
            Ok(())
        }
        _ => Err(NotStorable("a value of another kind")),
    }
}

fn put_extremes(
    w: &mut Vec<u8>,
    count: u64,
    min: Option<f64>,
    max: Option<f64>,
) -> Result<(), NotStorable> {
    match (count, min, max) {
        (0, None, None) => Ok(()),
        (1.., Some(lo), Some(hi)) if lo.total_cmp(&hi) != Ordering::Greater => {
            put_f64(w, lo)?;
            put_f64(w, hi)
        }
        _ => Err(NotStorable("min and max do not match the count")),
    }
}

fn put_exact(w: &mut Vec<u8>, x: &Exact) -> Result<(), NotStorable> {
    let Some(d) = dyadic(x).map_err(|()| NotStorable("an exact field that is not dyadic"))? else {
        w.push(0);
        return Ok(());
    };
    let bits = i64::try_from(d.mantissa.bits()).unwrap_or(i64::MAX);
    if d.exp < MIN_EXP || d.exp.saturating_add(bits) > MAX_BITS {
        return Err(NotStorable("an exact field out of range"));
    }
    w.push(if d.negative { 2 } else { 1 });
    w.extend_from_slice(&i16::try_from(d.exp).expect("bounded").to_le_bytes());
    let m = d.mantissa.to_bytes_le();
    put_uv(w, m.len() as u128);
    w.extend_from_slice(&m);
    Ok(())
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn invalid(&self, at: usize, what: &'static str) -> DecodeError {
        DecodeError::Invalid { at, what }
    }

    fn take(&mut self, n: usize) -> Result<&[u8], DecodeError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|e| *e <= self.b.len())
            .ok_or(DecodeError::Truncated(self.at))?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    /// Minimal unsigned LEB128 of at most `max_bits` bits.
    fn uv(&mut self, max_bits: u32) -> Result<u128, DecodeError> {
        let at = self.at;
        let mut v: u128 = 0;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            let low = u128::from(byte & 0x7F);
            if shift >= max_bits || low >> (max_bits - shift).min(7) != 0 {
                return Err(self.invalid(at, "integer out of range"));
            }
            v |= low << shift;
            if byte & 0x80 == 0 {
                if byte == 0 && shift > 0 {
                    return Err(self.invalid(at, "integer not in its shortest form"));
                }
                return Ok(v);
            }
            shift += 7;
        }
    }

    fn uv64(&mut self) -> Result<u64, DecodeError> {
        Ok(self.uv(64)? as u64)
    }

    fn uv_i128(&mut self) -> Result<i128, DecodeError> {
        Ok(self.uv(127)? as i128)
    }

    fn f64(&mut self) -> Result<f64, DecodeError> {
        let at = self.at;
        let v = f64::from_bits(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ));
        if !v.is_finite() {
            return Err(self.invalid(at, "a value that is not finite"));
        }
        Ok(v)
    }

    fn key(&mut self) -> Result<StateKey, DecodeError> {
        let at = self.at;
        match self.u8()? {
            0 => Ok(StateKey::Bool(false)),
            1 => Ok(StateKey::Bool(true)),
            2 => Ok(StateKey::State(u16::from_le_bytes(
                self.take(2)?.try_into().expect("2 bytes"),
            ))),
            _ => Err(self.invalid(at, "unknown state tag")),
        }
    }

    fn value(&mut self, kind: Kind) -> Result<Value, DecodeError> {
        match kind {
            Kind::Gauge | Kind::Counter | Kind::Interval => Ok(Value::F64(self.f64()?)),
            Kind::State => Ok(match self.key()? {
                StateKey::Bool(b) => Value::Bool(b),
                StateKey::State(i) => Value::State(i),
            }),
            Kind::Text => {
                let len = usize::from(self.u8()?);
                let at = self.at;
                let s = std::str::from_utf8(self.take(len)?).map_err(|_| DecodeError::Invalid {
                    at,
                    what: "text that is not UTF-8",
                })?;
                Ok(Value::Text(s.to_owned()))
            }
        }
    }

    fn extremes(
        &mut self,
        count: u64,
        first: &Option<(i64, Value)>,
        last: &Option<(i64, Value)>,
    ) -> Result<(Option<f64>, Option<f64>), DecodeError> {
        if count == 0 {
            return Ok((None, None));
        }
        let at = self.at;
        let lo = self.f64()?;
        let hi = self.f64()?;
        let within = |v: &Option<(i64, Value)>| {
            v.as_ref().and_then(|(_, v)| v.as_f64()).is_some_and(|x| {
                lo.total_cmp(&x) != Ordering::Greater && x.total_cmp(&hi) != Ordering::Greater
            })
        };
        if !within(first) || !within(last) {
            return Err(self.invalid(at, "first or last outside min and max"));
        }
        if count == 1 && lo.to_bits() != hi.to_bits() {
            return Err(self.invalid(at, "min and max differ with one reading"));
        }
        Ok((Some(lo), Some(hi)))
    }

    fn exact(&mut self) -> Result<Exact, DecodeError> {
        let at = self.at;
        let negative = match self.u8()? {
            0 => return Ok(Exact::zero()),
            1 => false,
            2 => true,
            _ => return Err(self.invalid(at, "unknown sign of an exact number")),
        };
        let exp = i64::from(i16::from_le_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ));
        let at_len = self.at;
        let len = self.uv64()?;
        if len == 0 || len > MAX_MANTISSA_BYTES {
            return Err(self.invalid(at_len, "mantissa length out of range"));
        }
        let m = self.take(len as usize)?;
        if m[0] & 1 == 0 || m[m.len() - 1] == 0 {
            return Err(self.invalid(at_len, "mantissa not odd or not in its shortest form"));
        }
        let mantissa = BigUint::from_bytes_le(m);
        let bits = i64::try_from(mantissa.bits()).unwrap_or(i64::MAX);
        if exp < MIN_EXP || exp.saturating_add(bits) > MAX_BITS {
            return Err(self.invalid(at, "exact number out of range"));
        }
        Ok(from_dyadic(Dyadic {
            negative,
            mantissa,
            exp,
        }))
    }
}
