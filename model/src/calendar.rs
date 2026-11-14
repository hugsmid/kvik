//! Calendar days and months (spec §5.7) and calendar windows (§6).
//!
//! Facts and the UTC levels are in UTC; a zone only defines calendar days and
//! months. A calendar **result** is a window evaluation over its span, edge
//! stretches included; stored calendar nodes are only an input to it.
//!
//! - [`Tzdb`] is the time-zone database in use, with its version. Zones are
//!   identified by [`ZoneId`], which resolves IANA links.
//! - [`Zoning`] takes local days either in the site zone, an effective-dated
//!   log of zone entries in which each local day uses the zone in force at its
//!   start (§12 D3; see `OPEN-QUESTIONS.md` Q-75), or in one named zone for the
//!   whole span.
//! - [`CalendarFile`] holds a series' stored day and month nodes, each with the
//!   zones its days were taken in, and the tzdata version they were built with.
//! - [`evaluate`] gives a calendar result: a window over the span, taking a
//!   stored node as an input only where every day it covers was taken in the
//!   zone of the query and its tzdata version is the current one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;

use crate::summary::{Node, merge_all};
use crate::tree::{SeriesTree, TreeError};
use crate::types::{MINUTE, Nanos, QUARTER, floor_to};

/// Identity of a zone after resolving IANA links (§5.7: zones are compared by
/// canonical name). The bundled database exposes no link table, but a link
/// shares its target's data, so the identity is a fingerprint of the zone's
/// bundled rules: the initial offset and abbreviation and every transition
/// (instant, offset, DST flag, abbreviation) up to 2200. Distinct zones of
/// the bundled data differ in these (most in their local mean time); two
/// zones whose rules are identical would count as one.
pub type ZoneId = u64;

/// The time-zone database the engine uses, with its version (§5.7: the tzdata
/// version is recorded per calendar file). The bundled database, or one that
/// a later release changed: [`Tzdb::with_rules`] replaces the rules of a zone
/// (and of every name linked to it), keeping its identity.
#[derive(Clone, Debug)]
pub struct Tzdb {
    version: String,
    rules: BTreeMap<ZoneId, TimeZone>,
}

impl Default for Tzdb {
    fn default() -> Self {
        Tzdb::bundled()
    }
}

impl Tzdb {
    /// The bundled database.
    pub fn bundled() -> Tzdb {
        Tzdb {
            version: "bundled".into(),
            rules: BTreeMap::new(),
        }
    }

    /// A later release `version` of this database in which the zone `name`,
    /// and every name linked to it, follows `rules`.
    pub fn with_rules(
        &self,
        version: &str,
        name: &str,
        rules: TimeZone,
    ) -> Result<Tzdb, TreeError> {
        let id = zone_id(name)?;
        let mut db = self.clone();
        db.version = version.into();
        db.rules.insert(id, rules);
        Ok(db)
    }

    /// The version.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The zone's identity, after resolving links.
    pub fn id(&self, name: &str) -> Result<ZoneId, TreeError> {
        zone_id(name)
    }

    /// The zone `name` under this database's rules.
    pub fn zone(&self, name: &str) -> Result<TimeZone, TreeError> {
        let id = zone_id(name)?;
        Ok(match self.rules.get(&id) {
            Some(tz) => tz.clone(),
            None => bundled(name)?,
        })
    }
}

fn bundled(name: &str) -> Result<TimeZone, TreeError> {
    TimeZone::get(name).map_err(|_| TreeError::UnknownZone(name.to_string()))
}

fn zone_id(name: &str) -> Result<ZoneId, TreeError> {
    static IDS: OnceLock<Mutex<BTreeMap<String, ZoneId>>> = OnceLock::new();
    let ids = IDS.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some(id) = ids.lock().expect("zone ids").get(name) {
        return Ok(*id);
    }
    let tz = bundled(name)?;
    let horizon: Timestamp = "2200-01-01T00:00:00Z".parse().expect("valid instant");
    let mut h = Fnv::default();
    let initial = tz.to_offset_info(Timestamp::MIN);
    h.write(&initial.offset().seconds().to_le_bytes());
    h.write(initial.abbreviation().as_bytes());
    h.write(&[0]);
    for tr in tz.following(Timestamp::MIN) {
        if tr.timestamp() >= horizon {
            break;
        }
        h.write(&tr.timestamp().as_second().to_le_bytes());
        h.write(&tr.offset().seconds().to_le_bytes());
        h.write(&[u8::from(tr.dst().is_dst())]);
        h.write(tr.abbreviation().as_bytes());
        h.write(&[0]);
    }
    let id = h.0;
    ids.lock().expect("zone ids").insert(name.to_string(), id);
    Ok(id)
}

struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

fn nanos(ts: Timestamp) -> Nanos {
    i64::try_from(ts.as_nanosecond()).expect("instant in range")
}

fn cal(e: jiff::Error) -> TreeError {
    TreeError::Calendar(e.to_string())
}

/// UTC span `[start, end)` of local `date` in `zone`, wherever its edges fall.
/// A day whose local midnight does not exist starts at its first instant
/// (§5.7), and a skipped date has an empty span.
pub fn day_span(zone: &TimeZone, date: Date) -> Result<(Nanos, Nanos), TreeError> {
    let at = |d: Date| -> Result<Nanos, TreeError> {
        Ok(nanos(d.to_zoned(zone.clone()).map_err(cal)?.timestamp()))
    };
    let start = at(date)?;
    let end = at(date.tomorrow().map_err(cal)?)?;
    Ok((start, end.max(start)))
}

/// Whether both edges of a span fall on 15-minute boundaries, so that a
/// calendar node may be stored for it (§5.7).
pub fn aligned(span: (Nanos, Nanos)) -> bool {
    floor_to(span.0, QUARTER) == span.0 && floor_to(span.1, QUARTER) == span.1
}

/// A calendar month.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Month {
    /// Year.
    pub year: i16,
    /// Month, 1–12.
    pub month: i8,
}

impl Month {
    /// The month holding `date`.
    pub fn of(date: Date) -> Month {
        Month {
            year: date.year(),
            month: date.month(),
        }
    }

    /// Its local dates, in order.
    pub fn dates(self) -> Result<Vec<Date>, TreeError> {
        let first = Date::new(self.year, self.month, 1).map_err(cal)?;
        Ok((0..first.days_in_month())
            .map(|i| {
                first
                    .checked_add(jiff::Span::new().days(i))
                    .expect("in range")
            })
            .collect())
    }

    /// The month `k` months after this one (before it for negative `k`).
    pub fn plus(self, k: i32) -> Result<Month, TreeError> {
        let first = Date::new(self.year, self.month, 1).map_err(cal)?;
        Ok(Month::of(
            first
                .checked_add(jiff::Span::new().months(k))
                .map_err(cal)?,
        ))
    }
}

/// A local day placed on the UTC timeline: its date, the zone it was taken
/// in and its span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Day {
    /// Local date.
    pub date: Date,
    /// Name of the zone the day was taken in.
    pub zone: String,
    /// Identity of that zone.
    pub zone_id: ZoneId,
    /// UTC span start.
    pub start: Nanos,
    /// UTC span end.
    pub end: Nanos,
}

/// How local days are taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Zoning {
    /// The site zone (§5.7): `initial` before the first entry, then each entry
    /// from its effective time (a later entry with the same effective time
    /// replaces an earlier one). Each local day uses the zone in force at its
    /// start. Where offsets differ that rule can fit a date in two zones or in
    /// none (Q-75); the model takes the latest entry under whose zone the
    /// day starts at or after the entry's effective time, the initial zone
    /// when there is none.
    Site {
        /// Zone before the first entry.
        initial: String,
        /// Entries by effective time.
        entries: BTreeMap<Nanos, String>,
    },
    /// One named IANA zone for the whole span, with no effective dating.
    Named(String),
}

impl Zoning {
    /// The local day `date`.
    pub fn day(&self, db: &Tzdb, date: Date) -> Result<Day, TreeError> {
        let place = |name: &str| -> Result<Day, TreeError> {
            let (start, end) = day_span(&db.zone(name)?, date)?;
            Ok(Day {
                date,
                zone: name.to_string(),
                zone_id: db.id(name)?,
                start,
                end,
            })
        };
        match self {
            Zoning::Named(name) => place(name),
            Zoning::Site { initial, entries } => {
                for (effective, name) in entries.iter().rev() {
                    let d = place(name)?;
                    if d.start >= *effective {
                        return Ok(d);
                    }
                }
                place(initial)
            }
        }
    }

    /// The local days of `month`, in order.
    pub fn month(&self, db: &Tzdb, month: Month) -> Result<Vec<Day>, TreeError> {
        month
            .dates()?
            .into_iter()
            .map(|d| self.day(db, d))
            .collect()
    }

    /// The zone in force at `x`: for the site zone the latest entry effective
    /// at or before `x`.
    pub fn zone_at(&self, x: Nanos) -> &str {
        match self {
            Zoning::Named(name) => name,
            Zoning::Site { initial, entries } => {
                entries.range(..=x).next_back().map_or(initial, |(_, z)| z)
            }
        }
    }

    fn candidates(&self, db: &Tzdb, x: Nanos) -> Result<Vec<Date>, TreeError> {
        let tz = db.zone(self.zone_at(x))?;
        let d0 = Timestamp::from_nanosecond(i128::from(x))
            .map_err(cal)?
            .to_zoned(tz)
            .date();
        (-2..=2)
            .map(|k| d0.checked_add(jiff::Span::new().days(k)).map_err(cal))
            .collect()
    }

    /// The local date at `x` ("today" for a calendar window): the latest
    /// date whose day starts at or before `x`.
    pub fn date_at(&self, db: &Tzdb, x: Nanos) -> Result<Date, TreeError> {
        let mut best = None;
        for d in self.candidates(db, x)? {
            if self.day(db, d)?.start <= x {
                best = Some(d);
            }
        }
        best.ok_or_else(|| TreeError::Calendar(format!("no local date at {x}")))
    }

    /// The local days whose spans overlap `[from, to)`, searched around both
    /// ends (spans longer than a few days do not occur).
    pub fn days_overlapping(
        &self,
        db: &Tzdb,
        from: Nanos,
        to: Nanos,
    ) -> Result<Vec<Day>, TreeError> {
        let mut dates: BTreeSet<Date> = BTreeSet::new();
        dates.extend(self.candidates(db, from)?);
        dates.extend(self.candidates(db, to.max(from + 1) - 1)?);
        let mut out = Vec::new();
        for d in dates {
            let day = self.day(db, d)?;
            if day.start < to && from < day.end {
                out.push(day);
            }
        }
        Ok(out)
    }
}

/// A stored calendar node (§5.1, §5.7): a day or a month, with the zone each
/// of its days was taken in.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredNode {
    /// The zone of each day it covers, in order.
    pub zones: Vec<ZoneId>,
    /// UTC span start.
    pub start: Nanos,
    /// UTC span end.
    pub end: Nanos,
    /// The node: the merge of its 15-minute nodes, or of its days.
    pub node: Node,
}

/// A series' calendar file (§5.1): its stored day and month nodes and the
/// tzdata version they were built with.
#[derive(Clone, Debug, PartialEq)]
pub struct CalendarFile {
    /// The tzdata version.
    pub tzdata: String,
    /// Day nodes by local date.
    pub days: BTreeMap<Date, StoredNode>,
    /// Month nodes.
    pub months: BTreeMap<Month, StoredNode>,
}

impl CalendarFile {
    /// An empty file.
    pub fn new(db: &Tzdb) -> CalendarFile {
        CalendarFile {
            tzdata: db.version().to_string(),
            days: BTreeMap::new(),
            months: BTreeMap::new(),
        }
    }

    /// Builds the file from the series' tree (rebuild). A calendar node is
    /// stored only for a span whose edges fall on 15-minute boundaries and for
    /// a series that stores the 15-minute level (§5.1 density rule; for an
    /// interval series §5.4), and only where it holds data. A month node is
    /// stored only where its days are contiguous: around a site-zone change
    /// whose offset differs they are not (Q-75).
    pub fn build(tree: &SeriesTree, zoning: &Zoning, db: &Tzdb) -> Result<CalendarFile, TreeError> {
        let mut file = CalendarFile::new(db);
        if !tree.stores(1) {
            return Ok(file);
        }
        let mut dates = BTreeSet::new();
        for q in tree.stored(1) {
            for d in zoning.days_overlapping(db, q.start, q.end)? {
                dates.insert(d.date);
            }
        }
        let months: BTreeSet<Month> = dates.iter().map(|d| Month::of(*d)).collect();
        for d in dates {
            file.refresh_day(tree, zoning, db, d)?;
        }
        for m in months {
            file.refresh_month(tree, zoning, db, m)?;
        }
        Ok(file)
    }

    /// Recomputes the calendar nodes over the minute starting at `minute`
    /// after its 15-minute node changed (§5.6).
    pub fn refresh(
        &mut self,
        tree: &SeriesTree,
        zoning: &Zoning,
        db: &Tzdb,
        minute: Nanos,
    ) -> Result<(), TreeError> {
        if !tree.stores(1) {
            return Ok(());
        }
        let q = floor_to(minute, QUARTER);
        let mut months = BTreeSet::new();
        for d in zoning.days_overlapping(db, q, q + QUARTER)? {
            self.refresh_day(tree, zoning, db, d.date)?;
            months.insert(Month::of(d.date));
        }
        for m in months {
            self.refresh_month(tree, zoning, db, m)?;
        }
        Ok(())
    }

    fn refresh_day(
        &mut self,
        tree: &SeriesTree,
        zoning: &Zoning,
        db: &Tzdb,
        date: Date,
    ) -> Result<(), TreeError> {
        match day_node(tree, &zoning.day(db, date)?) {
            Some(n) if !n.node.is_empty() => {
                self.days.insert(date, n);
            }
            _ => {
                self.days.remove(&date);
            }
        }
        Ok(())
    }

    fn refresh_month(
        &mut self,
        tree: &SeriesTree,
        zoning: &Zoning,
        db: &Tzdb,
        month: Month,
    ) -> Result<(), TreeError> {
        let days = zoning.month(db, month)?;
        let contiguous = days.windows(2).all(|w| w[0].end == w[1].start);
        let nodes: Option<Vec<StoredNode>> = days.iter().map(|d| day_node(tree, d)).collect();
        match nodes {
            Some(nodes) if contiguous && nodes.iter().any(|n| !n.node.is_empty()) => {
                let (start, end) = (days[0].start, days[days.len() - 1].end);
                let mut node = merge_all(&tree.params, start, nodes.iter().map(|n| &n.node));
                node.start = start;
                node.end = end;
                self.months.insert(
                    month,
                    StoredNode {
                        zones: days.iter().map(|d| d.zone_id).collect(),
                        start,
                        end,
                        node,
                    },
                );
            }
            _ => {
                self.months.remove(&month);
            }
        }
        Ok(())
    }

    /// The local dates whose stored day nodes differ between two files.
    pub fn changed_days(&self, other: &CalendarFile) -> Vec<Date> {
        let dates: BTreeSet<&Date> = self.days.keys().chain(other.days.keys()).collect();
        dates
            .into_iter()
            .filter(|d| self.days.get(d) != other.days.get(d))
            .copied()
            .collect()
    }
}

/// The calendar node for a day: the merge of its 15-minute nodes, `None`
/// where none may be stored (edges off 15-minute boundaries, or no stored
/// 15-minute level).
fn day_node(tree: &SeriesTree, day: &Day) -> Option<StoredNode> {
    if !tree.stores(1) || !aligned((day.start, day.end)) {
        return None;
    }
    let quarters: Vec<Node> = (0..(day.end - day.start) / QUARTER)
        .map(|i| tree.node(1, day.start + i * QUARTER))
        .collect();
    let mut node = merge_all(&tree.params, day.start, quarters.iter());
    node.start = day.start;
    node.end = day.end;
    Some(StoredNode {
        zones: vec![day.zone_id],
        start: day.start,
        end: day.end,
        node,
    })
}

/// A calendar period.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Period {
    /// A local day.
    Day(Date),
    /// A local month.
    Month(Month),
}

/// A calendar window (§6 Windows), taken at `now`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalendarWindow {
    /// Since local midnight: from the start of today to `now`.
    SinceMidnight,
    /// This month: from the start of the current month to `now`.
    ThisMonth,
    /// The local day `k` days after today (before it for negative `k`),
    /// whole.
    Day(i32),
    /// The month `k` months after the current one, whole.
    Month(i32),
    /// A named period, whole.
    Period(Period),
}

/// The span of a calendar window and its local days, in order.
pub fn window_span(
    zoning: &Zoning,
    db: &Tzdb,
    w: CalendarWindow,
    now: Nanos,
) -> Result<(Nanos, Nanos, Vec<Day>), TreeError> {
    let whole = |days: Vec<Day>| -> (Nanos, Nanos, Vec<Day>) {
        let (s, e) = (days[0].start, days[days.len() - 1].end);
        (s, e.max(s), days)
    };
    let today = || zoning.date_at(db, now);
    Ok(match w {
        CalendarWindow::Period(Period::Day(d)) => whole(vec![zoning.day(db, d)?]),
        CalendarWindow::Period(Period::Month(m)) => whole(zoning.month(db, m)?),
        CalendarWindow::Day(k) => {
            let d = today()?
                .checked_add(jiff::Span::new().days(k))
                .map_err(cal)?;
            whole(vec![zoning.day(db, d)?])
        }
        CalendarWindow::Month(k) => whole(zoning.month(db, Month::of(today()?).plus(k)?)?),
        CalendarWindow::SinceMidnight => {
            let day = zoning.day(db, today()?)?;
            (day.start, now.max(day.start), vec![day])
        }
        CalendarWindow::ThisMonth => {
            let today = today()?;
            let days: Vec<Day> = zoning
                .month(db, Month::of(today))?
                .into_iter()
                .filter(|d| d.date <= today)
                .collect();
            (days[0].start, now.max(days[0].start), days)
        }
    })
}

/// The stored calendar nodes that may be inputs to a calendar result over
/// `days` (§5.7): only from a file of the current tzdata version, only nodes
/// whose every day was taken in the zone the query takes it in and whose span
/// is the day's, and only whole inside `[from, limit)`. A month node is taken
/// when `month` is the whole period; otherwise day nodes.
pub fn inputs(
    file: Option<&CalendarFile>,
    db: &Tzdb,
    days: &[Day],
    month: Option<Month>,
    from: Nanos,
    limit: Nanos,
) -> Vec<Node> {
    let Some(file) = file.filter(|f| f.tzdata == db.version()) else {
        return Vec::new();
    };
    let inside = |n: &StoredNode| from <= n.start && n.end <= limit;
    if let Some(m) = month
        && let Some(n) = file.months.get(&m)
        && inside(n)
        && days.first().map(|d| d.start) == Some(n.start)
        && days.last().map(|d| d.end) == Some(n.end)
        && n.zones == days.iter().map(|d| d.zone_id).collect::<Vec<_>>()
    {
        return vec![n.node.clone()];
    }
    let mut out: Vec<Node> = Vec::new();
    for d in days {
        if let Some(n) = file.days.get(&d.date)
            && inside(n)
            && n.zones == [d.zone_id]
            && (n.start, n.end) == (d.start, d.end)
            && out.last().is_none_or(|p| p.end <= n.start)
        {
            out.push(n.node.clone());
        }
    }
    out
}

/// A calendar result (§5.7): the window `[from, to)` evaluated at `now` over
/// the UTC data, edge stretches included, with `inputs` (stored calendar
/// nodes, from [`inputs`]) in place of the nodes they cover.
pub fn evaluate(tree: &SeriesTree, from: Nanos, to: Nanos, now: Nanos, inputs: &[Node]) -> Node {
    tree.window_with(from, to, now, inputs)
}

/// Whether a span's edges fall on minute boundaries.
pub fn on_minutes(from: Nanos, to: Nanos) -> bool {
    floor_to(from, MINUTE) == from && floor_to(to, MINUTE) == to
}
