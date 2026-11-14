//! Time types of the interface (`docs/api.md` Core types): only std types
//! appear in signatures. Conversions to and from `jiff` are behind the optional
//! feature `jiff`.

use std::fmt;
use std::ops::{Add, Sub};
use std::str::FromStr;
use std::time::Duration;

/// An instant: nanoseconds since the Unix epoch, UTC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// The Unix epoch.
    pub const EPOCH: Timestamp = Timestamp(0);

    /// Whole seconds since the epoch.
    pub const fn from_secs(secs: i64) -> Self {
        Timestamp(secs * 1_000_000_000)
    }

    /// Nanoseconds since the epoch.
    pub const fn as_nanos(self) -> i64 {
        self.0
    }

    /// `self + span`, or `None` on overflow.
    pub fn checked_add(self, span: Span) -> Option<Timestamp> {
        self.0.checked_add(span.0).map(Timestamp)
    }
}

impl Add<Span> for Timestamp {
    type Output = Timestamp;
    /// # Panics
    /// Panics on overflow.
    fn add(self, rhs: Span) -> Timestamp {
        self.checked_add(rhs).expect("timestamp within range")
    }
}

impl Add<Duration> for Timestamp {
    type Output = Timestamp;
    /// # Panics
    /// Panics on overflow.
    fn add(self, rhs: Duration) -> Timestamp {
        self + Span::try_from(rhs).expect("duration within range")
    }
}

impl Sub for Timestamp {
    type Output = Span;
    fn sub(self, rhs: Timestamp) -> Span {
        Span(self.0 - rhs.0)
    }
}

impl fmt::Display for Timestamp {
    /// RFC 3339 in UTC.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match jiff::Timestamp::from_nanosecond(i128::from(self.0)) {
            Ok(t) => write!(f, "{t}"),
            Err(_) => write!(f, "{}ns", self.0),
        }
    }
}

/// The error of parsing a [`Timestamp`], [`LocalDate`] or [`LocalTime`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("cannot parse {0:?}")]
pub struct ParseError(String);

impl FromStr for Timestamp {
    type Err = ParseError;
    /// Parses RFC 3339, such as `2026-01-05T12:00:00Z`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t: jiff::Timestamp = s.parse().map_err(|_| ParseError(s.into()))?;
        i64::try_from(t.as_nanosecond())
            .map(Timestamp)
            .map_err(|_| ParseError(s.into()))
    }
}

/// A signed length of time in nanoseconds: window offsets relative to `now`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span(pub i64);

impl Span {
    /// Zero.
    pub const ZERO: Span = Span(0);

    /// Whole seconds, either sign.
    pub const fn from_secs(secs: i64) -> Self {
        Span(secs * 1_000_000_000)
    }

    /// The span's absolute length.
    pub fn unsigned_abs(self) -> Duration {
        Duration::from_nanos(self.0.unsigned_abs())
    }
}

impl std::ops::Neg for Span {
    type Output = Span;
    fn neg(self) -> Span {
        Span(-self.0)
    }
}

impl TryFrom<Duration> for Span {
    type Error = std::num::TryFromIntError;
    fn try_from(d: Duration) -> Result<Self, Self::Error> {
        i64::try_from(d.as_nanos()).map(Span)
    }
}

/// A half-open range of event time, `[from, to)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TimeRange {
    /// Start, inclusive.
    pub from: Timestamp,
    /// End, exclusive.
    pub to: Timestamp,
}

impl TimeRange {
    /// `[from, to)`.
    pub const fn new(from: Timestamp, to: Timestamp) -> Self {
        TimeRange { from, to }
    }
}

impl From<std::ops::Range<Timestamp>> for TimeRange {
    fn from(r: std::ops::Range<Timestamp>) -> Self {
        TimeRange::new(r.start, r.end)
    }
}

/// A local calendar date.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalDate {
    /// Year.
    pub year: i16,
    /// Month, 1–12.
    pub month: u8,
    /// Day of the month, from 1.
    pub day: u8,
}

impl LocalDate {
    /// A date; checked where it is used.
    pub const fn new(year: i16, month: u8, day: u8) -> Self {
        LocalDate { year, month, day }
    }

    pub(crate) fn to_jiff(self) -> Option<jiff::civil::Date> {
        let m = i8::try_from(self.month).ok()?;
        let d = i8::try_from(self.day).ok()?;
        jiff::civil::Date::new(self.year, m, d).ok()
    }
}

impl fmt::Display for LocalDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// A local time of day: nanoseconds since local midnight, below 24 hours.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalTime(u64);

impl LocalTime {
    /// Midnight.
    pub const MIDNIGHT: LocalTime = LocalTime(0);

    /// Hours, minutes and seconds; `None` outside a day.
    pub const fn from_hms(h: u8, m: u8, s: u8) -> Option<Self> {
        if h > 23 || m > 59 || s > 59 {
            return None;
        }
        Some(LocalTime(
            ((h as u64 * 60 + m as u64) * 60 + s as u64) * 1_000_000_000,
        ))
    }

    /// Nanoseconds since local midnight.
    pub const fn as_nanos(self) -> u64 {
        self.0
    }
}

/// An IANA time-zone name; checked against the tzdata release where used.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZoneName(String);

impl ZoneName {
    /// A zone name.
    pub fn new(name: impl Into<String>) -> Self {
        ZoneName(name.into())
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ZoneName {
    fn from(s: &str) -> Self {
        ZoneName::new(s)
    }
}

impl fmt::Display for ZoneName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(feature = "jiff")]
mod jiff_conversions {
    use super::{LocalDate, Span, Timestamp};

    impl TryFrom<jiff::Timestamp> for Timestamp {
        type Error = std::num::TryFromIntError;
        fn try_from(t: jiff::Timestamp) -> Result<Self, Self::Error> {
            i64::try_from(t.as_nanosecond()).map(Timestamp)
        }
    }

    impl From<Timestamp> for jiff::Timestamp {
        fn from(t: Timestamp) -> Self {
            jiff::Timestamp::from_nanosecond(i128::from(t.0)).expect("i64 nanoseconds are in range")
        }
    }

    impl TryFrom<jiff::SignedDuration> for Span {
        type Error = std::num::TryFromIntError;
        fn try_from(d: jiff::SignedDuration) -> Result<Self, Self::Error> {
            i64::try_from(d.as_nanos()).map(Span)
        }
    }

    impl From<jiff::civil::Date> for LocalDate {
        fn from(d: jiff::civil::Date) -> Self {
            // Months and days of a valid date are positive.
            LocalDate::new(d.year(), d.month().unsigned_abs(), d.day().unsigned_abs())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_print_and_add() {
        let t: Timestamp = "2026-01-05T12:00:00Z".parse().unwrap();
        assert_eq!(t, Timestamp::from_secs(1_767_614_400));
        assert_eq!(t.to_string(), "2026-01-05T12:00:00Z");
        assert_eq!(t + Span::from_secs(-60) + Duration::from_secs(60), t);
        assert_eq!((t + Span::from_secs(5)) - t, Span::from_secs(5));
        assert!("yesterday".parse::<Timestamp>().is_err());
    }

    #[test]
    fn local_dates_and_times() {
        assert!(LocalDate::new(2026, 2, 29).to_jiff().is_none());
        assert_eq!(LocalDate::new(2026, 3, 2).to_string(), "2026-03-02");
        assert_eq!(
            LocalTime::from_hms(5, 0, 0).unwrap().as_nanos(),
            5 * 3_600 * 1_000_000_000
        );
        assert!(LocalTime::from_hms(24, 0, 0).is_none());
    }
}
