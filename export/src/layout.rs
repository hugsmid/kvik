//! The partitioned path layout `site=<site>/area=<s1>/device=<s2>/metric=<s3>/month=<YYYY-MM>/<view>.parquet`
//! (`docs/export.md` Partitioned layout, Q-196 as amended, Q-670).

use std::fmt;

use jiff::Timestamp as JiffTimestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use kvik::{TimeRange, Timestamp, View};

use crate::ExportError;

/// The longest site label, in bytes.
pub const MAX_SITE_LEN: usize = 64;

/// Checks a site label: 1 to [`MAX_SITE_LEN`] bytes of `a`–`z`, `0`–`9`, `-` and `_`, starting with a letter, so
/// that `site=<site>` is the same folder on every file system and bucket and no reader takes it for a number.
pub fn check_site(site: &str) -> Result<(), ExportError> {
    let b = site.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= MAX_SITE_LEN
        && b[0].is_ascii_lowercase()
        && b.iter().all(|&c| kept(c));
    if ok {
        Ok(())
    } else {
        Err(ExportError::InvalidOptions {
            reason: "a site label is 1 to 64 bytes of a-z, 0-9, '-' and '_', starting with a letter",
        })
    }
}

/// The bytes a path segment keeps as they are; the same set a site label is drawn from.
fn kept(c: u8) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_'
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// One segment of a series name as one path segment: bytes of `a`–`z`, `0`–`9`, `-` and `_` kept, every other byte
/// as `%XX` in upper-case hex. Injective even on file systems that fold case, and reversed by [`unescape_segment`].
pub fn escape_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for &c in name.as_bytes() {
        if kept(c) {
            out.push(char::from(c));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(c >> 4)]));
            out.push(char::from(HEX[usize::from(c & 0xF)]));
        }
    }
    out
}

/// The name a segment of [`escape_segment`] stands for; `None` for a segment it never writes (a byte it would
/// escape, lower-case hex, an escape of a kept byte, or bytes that are not UTF-8).
pub fn unescape_segment(segment: &str) -> Option<String> {
    let b = segment.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let digit = |j: usize| {
                let d = *b.get(j)?;
                HEX.iter().position(|&h| h == d)
            };
            let c = u8::try_from(digit(i + 1)? * 16 + digit(i + 2)?).ok()?;
            if kept(c) {
                return None;
            }
            out.push(c);
            i += 3;
        } else if kept(b[i]) {
            out.push(b[i]);
            i += 1;
        } else {
            return None;
        }
    }
    String::from_utf8(out).ok()
}

/// A calendar month in UTC that holds an `i64` timestamp: the partition of a fact with `t` in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Month {
    year: i16,
    month: i8,
}

impl Month {
    /// The UTC month of `t` (Q-196: the month of `t` in UTC for every series, interval series included).
    pub fn of(t: Timestamp) -> Month {
        let date = JiffTimestamp::from_nanosecond(i128::from(t.0))
            .expect("every i64 nanosecond count is a jiff timestamp")
            .to_zoned(TimeZone::UTC)
            .date();
        Month {
            year: date.year(),
            month: date.month(),
        }
    }

    /// The year.
    pub fn year(self) -> i16 {
        self.year
    }

    /// The month, 1 to 12.
    pub fn month(self) -> i8 {
        self.month
    }

    /// The month's half-open range of `t`, clipped to `i64` nanoseconds.
    pub fn range(self) -> TimeRange {
        let next = if self.month == 12 {
            Month {
                year: self.year + 1,
                month: 1,
            }
        } else {
            Month {
                year: self.year,
                month: self.month + 1,
            }
        };
        TimeRange::new(month_start(self), month_start(next))
    }
}

/// The first nanosecond of a month, clipped to `i64`.
fn month_start(m: Month) -> Timestamp {
    let ns = Date::new(m.year, m.month, 1)
        .and_then(|d| d.to_zoned(TimeZone::UTC))
        .map(|z| z.timestamp().as_nanosecond())
        .expect("months of i64 timestamps are jiff dates");
    Timestamp(i64::try_from(ns).unwrap_or(if ns < 0 { i64::MIN } else { i64::MAX }))
}

impl fmt::Display for Month {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}", self.year, self.month)
    }
}

/// The segments `[area, device, metric]` of a series name; `None` unless it has exactly three, none empty (Q-670).
pub fn name_segments(name: &str) -> Option<[&str; 3]> {
    let mut parts = name.split('/');
    let segments = [parts.next()?, parts.next()?, parts.next()?];
    (parts.next().is_none() && segments.iter().all(|s| !s.is_empty())).then_some(segments)
}

pub(crate) fn checked_segments(name: &str) -> Result<[&str; 3], ExportError> {
    name_segments(name).ok_or(ExportError::UnsupportedSeries {
        reason: "a partitioned export needs a series name of three non-empty segments, area/device/metric (Q-670)",
    })
}

/// The folder of one series' month: `site=<site>/area=<s1>/device=<s2>/metric=<s3>/month=<YYYY-MM>`, each segment
/// escaped on its own. A name without three non-empty segments is refused (Q-670).
pub fn partition_dir(site: &str, series: &str, month: Month) -> Result<String, ExportError> {
    let [area, device, metric] = checked_segments(series)?;
    Ok(format!(
        "site={site}/area={}/device={}/metric={}/month={month}",
        escape_segment(area),
        escape_segment(device),
        escape_segment(metric)
    ))
}

/// The file name of a whole month's export in its folder, from its view: `current.parquet`, `history.parquet`,
/// `as_of-<generation>-<sequence>.parquet` or `as_of_time-<ns>.parquet`.
pub fn file_name(view: View) -> Result<String, ExportError> {
    Ok(format!("{}.parquet", view_stem(view)?))
}

/// The file name of a month clipped to `range`: `<view>.partial-<from_ns>_<to_ns>.parquet`, so it never replaces
/// the whole month's [`file_name`].
pub fn partial_file_name(view: View, range: TimeRange) -> Result<String, ExportError> {
    Ok(format!(
        "{}.partial-{}_{}.parquet",
        view_stem(view)?,
        range.from.0,
        range.to.0
    ))
}

fn view_stem(view: View) -> Result<String, ExportError> {
    Ok(match view {
        View::Current => "current".into(),
        View::History => "history".into(),
        View::AsOf(p) => format!("as_of-{}-{}", p.generation, p.sequence),
        View::AsOfTime(t) => format!("as_of_time-{}", t.0),
        _ => {
            return Err(ExportError::InvalidOptions {
                reason: "a view the export does not know",
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_digits_are_upper_case() {
        assert_eq!(escape_segment("a/b"), "a%2Fb");
        assert_eq!(unescape_segment("a%2fb"), None);
    }
}
