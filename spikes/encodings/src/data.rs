//! Deterministic synthetic per-second data.
//!
//! A [`Generator`] simulates the log of a store with `n` series, all reporting
//! once per second, span by span (a span is normally one UTC day). Within each
//! second the readings of all series arrive in a random order and receive
//! consecutive positions, so a series' positions step by roughly the number of
//! series. Each span is then compacted per series into a chunk of current facts
//! and a chunk of revisions, as `docs/spec.md` §7 describes.
//!
//! Series `i` has shape `shapes[i % shapes.len()]`, from [`DECIMAL_SHAPES`]
//! or [`NON_DECIMAL_SHAPES`]. Values are quantised to a device resolution by
//! dividing an integer by a power of ten, which gives the same `f64` as
//! parsing the decimal text a device sends; the non-decimal shapes then
//! compute from such values. With [`Generator::integers`], the decimal shapes
//! that have a fixed scale are emitted as `i64` with that scale instead.

use crate::{
    Chunk, FLAG_HOST_TIME, FLAG_LATE, FLAG_NO_VALUE, ValueKind,
    quality::{BAD, GOOD, SUSPECT},
};

/// One second in nanoseconds.
pub const SECOND: i64 = 1_000_000_000;
/// One day in seconds.
pub const DAY_SECONDS: i64 = 86_400;
/// Start of the first span: 2026-01-01T00:00:00Z in nanoseconds.
pub const START: i64 = 1_767_225_600 * SECOND;
/// The lateness bound (default 5 minutes, `docs/spec.md` §2.1).
const LATENESS: i64 = 300;

/// The shapes of synthetic series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// Slowly varying temperature-like gauge with noise, 0.01 resolution.
    Temperature,
    /// Monotonically increasing energy counter in kWh, 0.001 resolution.
    Energy,
    /// Two-state boolean with a mean dwell of 10 minutes.
    Switch,
    /// Host-stamped gauge (millisecond jitter at nanosecond resolution), 0.1
    /// resolution, with outages, occasional bad readings and suspect runs.
    Gappy,
    /// Dense integer-valued gauge with occasional corrections of an earlier
    /// second (revisions) arriving 5 s to 15 min later.
    Revised,
    /// Temperature read in °F at 0.1 resolution and converted to °C: almost
    /// no value is decimal.
    Converted,
    /// As `Temperature`, but about 0.1 % of values are computed (the mean of
    /// the last three readings) and so not decimal.
    Mixed,
}

/// Every shape, in table order.
pub const SHAPES: [Shape; 7] = [
    Shape::Temperature,
    Shape::Energy,
    Shape::Switch,
    Shape::Gappy,
    Shape::Revised,
    Shape::Converted,
    Shape::Mixed,
];

/// Shapes whose values are all decimal.
pub const DECIMAL_SHAPES: [Shape; 5] = [
    Shape::Temperature,
    Shape::Energy,
    Shape::Switch,
    Shape::Gappy,
    Shape::Revised,
];

/// Shapes with values that are not decimal.
pub const NON_DECIMAL_SHAPES: [Shape; 2] = [Shape::Converted, Shape::Mixed];

impl Shape {
    /// Short name for tables.
    pub fn name(self) -> &'static str {
        match self {
            Shape::Temperature => "temperature",
            Shape::Energy => "energy counter",
            Shape::Switch => "boolean",
            Shape::Gappy => "gappy gauge",
            Shape::Revised => "dense with revisions",
            Shape::Converted => "°F→°C",
            Shape::Mixed => "0.1 % computed",
        }
    }

    /// The decimal scale of a shape emitted as `i64`, if it has one.
    pub fn integer_scale(self) -> Option<i32> {
        match self {
            Shape::Temperature => Some(2),
            Shape::Energy => Some(3),
            Shape::Revised => Some(0),
            _ => None,
        }
    }
}

/// SplitMix64.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator from a seed.
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `[0, n)`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Uniform in `[a, b)`.
    pub fn range(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.unit()
    }

    /// `true` with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    /// Standard normal (Box–Muller).
    pub fn normal(&mut self) -> f64 {
        let u = 1.0 - self.unit();
        let v = self.unit();
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

fn quantise(x: f64, scale: f64) -> f64 {
    (x * scale).round() / scale
}

struct Reading {
    t: i64,
    value: u64,
    quality: u8,
    flags: u16,
}

struct SeriesGen {
    shape: Shape,
    rng: Rng,
    base: f64,
    amp: f64,
    phase: f64,
    walk: f64,
    level: f64,
    power: f64,
    boost: f64,
    boost_left: u32,
    wh: f64,
    state: bool,
    gap_left: u32,
    suspect_left: u32,
    last: [f64; 3],
}

impl SeriesGen {
    fn new(shape: Shape, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let base = rng.range(18.0, 24.0);
        let amp = rng.range(1.0, 4.0);
        let phase = rng.range(0.0, std::f64::consts::TAU);
        let level = match shape {
            Shape::Gappy => rng.range(40.0, 60.0),
            _ => rng.range(300.0, 900.0),
        };
        let power = rng.range(150.0, 400.0);
        let wh = rng.range(1.0e7, 5.0e7).floor();
        SeriesGen {
            shape,
            rng,
            base,
            amp,
            phase,
            walk: 0.0,
            level,
            power,
            boost: 0.0,
            boost_left: 0,
            wh,
            state: false,
            gap_left: 0,
            suspect_left: 0,
            last: [0.0; 3],
        }
    }

    /// The reading for absolute second `s` (seconds since [`START`]), and an
    /// optional correction of it: (arrival delay in seconds, corrected value).
    fn reading(&mut self, s: i64) -> (Option<Reading>, Option<(i64, u64)>) {
        let t = START + s * SECOND;
        let r = &mut self.rng;
        match self.shape {
            Shape::Temperature | Shape::Mixed | Shape::Converted => {
                self.walk = 0.9995 * self.walk + 0.003 * r.normal();
                let day = std::f64::consts::TAU * (s as f64) / DAY_SECONDS as f64;
                let x =
                    self.base + self.amp * (day + self.phase).sin() + self.walk + 0.01 * r.normal();
                let v = match self.shape {
                    Shape::Converted => (quantise(x * 1.8 + 32.0, 10.0) - 32.0) * 5.0 / 9.0,
                    _ => quantise(x, 100.0),
                };
                self.last = [self.last[1], self.last[2], v];
                let v = if self.shape == Shape::Mixed && s > 2 && r.chance(0.001) {
                    (self.last[0] + self.last[1] + self.last[2]) / 3.0
                } else {
                    v
                };
                (Some(good(t, v.to_bits(), 0)), None)
            }
            Shape::Energy => {
                if self.boost_left == 0 && r.chance(1.0 / 1800.0) {
                    self.boost = r.range(500.0, 3000.0);
                    self.boost_left = 300 + r.below(3300) as u32;
                }
                let mut p = self.power + 20.0 * r.normal();
                if self.boost_left > 0 {
                    p += self.boost;
                    self.boost_left -= 1;
                }
                self.wh += p.max(0.0) / 3600.0;
                let v = self.wh.floor() / 1000.0;
                (Some(good(t, v.to_bits(), 0)), None)
            }
            Shape::Switch => {
                if r.chance(1.0 / 600.0) {
                    self.state = !self.state;
                }
                (Some(good(t, u64::from(self.state), 0)), None)
            }
            Shape::Gappy => {
                if self.gap_left > 0 {
                    self.gap_left -= 1;
                    return (None, None);
                }
                if r.chance(1.0 / 3600.0) {
                    self.gap_left = 60 + r.below(1740) as u32;
                    return (None, None);
                }
                let t = t + r.below(8_000_000) as i64;
                self.level = (self.level + 0.02 * r.normal()).clamp(20.0, 90.0);
                if r.chance(0.001) {
                    let flags = FLAG_HOST_TIME | FLAG_NO_VALUE;
                    return (
                        Some(Reading {
                            t,
                            value: 0,
                            quality: BAD,
                            flags,
                        }),
                        None,
                    );
                }
                if self.suspect_left == 0 && r.chance(1.0 / 7200.0) {
                    self.suspect_left = 10 + r.below(110) as u32;
                }
                let quality = if self.suspect_left > 0 {
                    self.suspect_left -= 1;
                    SUSPECT
                } else {
                    GOOD
                };
                let v = quantise(self.level + 0.05 * r.normal(), 10.0);
                let reading = Reading {
                    t,
                    value: v.to_bits(),
                    quality,
                    flags: FLAG_HOST_TIME,
                };
                (Some(reading), None)
            }
            Shape::Revised => {
                self.level = (self.level + 2.0 * r.normal()).clamp(50.0, 5000.0);
                let v = quantise(self.level + 5.0 * r.normal(), 1.0);
                let correction = if r.chance(0.001) {
                    let delay = 5 + r.below(895) as i64;
                    let step = (1 + r.below(20)) as f64;
                    let c = if r.chance(0.5) { v + step } else { v - step };
                    Some((delay, c.to_bits()))
                } else {
                    None
                };
                (Some(good(t, v.to_bits(), 0)), correction)
            }
        }
    }
}

fn good(t: i64, value: u64, flags: u16) -> Reading {
    Reading {
        t,
        value,
        quality: GOOD,
        flags,
    }
}

/// One series' output for one span.
pub struct SeriesSpan {
    /// The series' shape.
    pub shape: Shape,
    /// Current facts, ordered by `t`.
    pub current: Chunk,
    /// Revisions (facts outranked within the span), ordered by `t`, position.
    pub revisions: Chunk,
    /// `received_at` of every fact: those of `current`, then of `revisions`.
    pub received_at: Vec<i64>,
}

/// Generates spans of synthetic log data for `n` series.
pub struct Generator {
    series: Vec<SeriesGen>,
    integers: bool,
    arrival: Rng,
    next_sequence: u64,
    span_seconds: i64,
    span_index: i64,
}

struct Arrival {
    series: usize,
    reading: Reading,
    offset: i64,
}

impl Generator {
    /// A generator for `n` series of the decimal shapes with spans of
    /// `span_seconds`.
    pub fn new(seed: u64, n: usize, span_seconds: i64) -> Self {
        Self::with_shapes(seed, n, span_seconds, &DECIMAL_SHAPES)
    }

    /// A generator for `n` series cycling through `shapes`.
    pub fn with_shapes(seed: u64, n: usize, span_seconds: i64, shapes: &[Shape]) -> Self {
        let series = (0..n)
            .map(|i| {
                let s = seed
                    .wrapping_mul(0x2545_f491_4f6c_dd1d)
                    .wrapping_add(i as u64 + 1);
                SeriesGen::new(shapes[i % shapes.len()], s)
            })
            .collect();
        Generator {
            series,
            integers: false,
            arrival: Rng::new(seed ^ 0x5eed),
            next_sequence: 1,
            span_seconds,
            span_index: 0,
        }
    }

    /// Emits the shapes with an [`Shape::integer_scale`] as `i64` values
    /// with that scale (`round(v × 10^scale)`), instead of `f64`.
    pub fn integers(mut self) -> Self {
        self.integers = true;
        self
    }

    /// Start of span `index` in nanoseconds.
    pub fn span_start(&self, index: i64) -> i64 {
        START + index * self.span_seconds * SECOND
    }

    /// Generates the next span.
    pub fn next_span(&mut self) -> Vec<SeriesSpan> {
        let span = self.span_seconds;
        let s0 = self.span_index * span;
        self.span_index += 1;
        let n = self.series.len();
        let mut logs: Vec<(Chunk, Vec<i64>)> = self
            .series
            .iter()
            .map(|g| {
                let kind = if g.shape == Shape::Switch {
                    ValueKind::State
                } else {
                    ValueKind::F64
                };
                let mut c = Chunk::new(kind);
                let cap = span as usize;
                c.t.reserve(cap);
                c.value.reserve(cap);
                c.quality.reserve(cap);
                c.flags.reserve(cap);
                c.position.reserve(cap);
                (c, Vec::with_capacity(cap))
            })
            .collect();
        let mut pending: Vec<Vec<(usize, Reading)>> = (0..span).map(|_| Vec::new()).collect();
        let mut events: Vec<Arrival> = Vec::with_capacity(n + 8);
        for k in 0..span {
            let s = s0 + k;
            events.clear();
            for (i, g) in self.series.iter_mut().enumerate() {
                let (reading, correction) = g.reading(s);
                if let Some(r) = reading {
                    if let Some((delay, value)) = correction
                        && k + delay < span
                    {
                        let flags = if delay > LATENESS { FLAG_LATE } else { 0 };
                        pending[(k + delay) as usize].push((
                            i,
                            Reading {
                                t: r.t,
                                value,
                                quality: GOOD,
                                flags,
                            },
                        ));
                    }
                    events.push(Arrival {
                        series: i,
                        reading: r,
                        offset: 0,
                    });
                }
            }
            for (i, r) in pending[k as usize].drain(..) {
                events.push(Arrival {
                    series: i,
                    reading: r,
                    offset: 0,
                });
            }
            for e in events.iter_mut() {
                // Arrival within the second, after any host-stamp jitter (< 8 ms).
                e.offset = 10_000_000 + self.arrival.below(980_000_000) as i64;
            }
            events.sort_by_key(|e| e.offset);
            let second = START + s * SECOND;
            for e in events.drain(..) {
                let (c, recv) = &mut logs[e.series];
                let r = e.reading;
                c.push(r.t, r.value, r.quality, r.flags, self.next_sequence);
                recv.push(second + e.offset);
                self.next_sequence += 1;
            }
        }
        let integers = self.integers;
        logs.into_iter()
            .zip(&self.series)
            .map(|((log, recv), g)| {
                let mut span = compact(g.shape, log, recv);
                if let (true, Some(scale)) = (integers, g.shape.integer_scale()) {
                    to_integers(&mut span.current, scale);
                    to_integers(&mut span.revisions, scale);
                }
                span
            })
            .collect()
    }
}

fn to_integers(c: &mut Chunk, scale: i32) {
    c.kind = ValueKind::I64;
    let p = 10f64.powi(scale);
    for (v, &f) in c.value.iter_mut().zip(&c.flags) {
        if f & FLAG_NO_VALUE == 0 {
            *v = (f64::from_bits(*v) * p).round() as i64 as u64;
        }
    }
}

/// Splits one series' facts (in position order) into current facts and
/// revisions: for each `t` the fact with the highest position is current.
fn compact(shape: Shape, log: Chunk, received_at: Vec<i64>) -> SeriesSpan {
    if log.t.windows(2).all(|w| w[0] < w[1]) {
        return SeriesSpan {
            shape,
            revisions: Chunk::new(log.kind),
            current: log,
            received_at,
        };
    }
    let mut order: Vec<usize> = (0..log.len()).collect();
    order.sort_by_key(|&i| (log.t[i], log.position[i]));
    let mut current = Chunk::new(log.kind);
    let mut revisions = Chunk::new(log.kind);
    let mut recv_cur = Vec::with_capacity(log.len());
    let mut recv_rev = Vec::new();
    for (j, &i) in order.iter().enumerate() {
        let last = order.get(j + 1).is_none_or(|&k| log.t[k] != log.t[i]);
        let (dst, recv) = if last {
            (&mut current, &mut recv_cur)
        } else {
            (&mut revisions, &mut recv_rev)
        };
        dst.push(
            log.t[i],
            log.value[i],
            log.quality[i],
            log.flags[i],
            log.position[i],
        );
        recv.push(received_at[i]);
    }
    recv_cur.extend(recv_rev);
    SeriesSpan {
        shape,
        current,
        revisions,
        received_at: recv_cur,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_well_formed() {
        let a = Generator::new(7, 10, 3600).next_span();
        let b = Generator::new(7, 10, 3600).next_span();
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.current, y.current);
            assert_eq!(x.revisions, y.revisions);
        }
        for s in &a {
            assert!(s.current.t.windows(2).all(|w| w[0] < w[1]));
            for i in 0..s.current.len() {
                let no_value = s.current.flags[i] & FLAG_NO_VALUE != 0;
                if no_value {
                    assert_eq!(s.current.value[i], 0);
                    assert_eq!(s.current.quality[i], BAD);
                } else if s.current.kind == ValueKind::F64 {
                    assert!(f64::from_bits(s.current.value[i]).is_finite());
                }
            }
            assert_eq!(s.received_at.len(), s.current.len() + s.revisions.len());
        }
    }
}
