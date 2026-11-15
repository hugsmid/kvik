//! One exported row: a fact as the export file holds it.

use kvik::{Fact, FactFlags, Position, Quality, SeriesId, Timestamp, Value};

/// A fact's engine flags (spec §2.2), one boolean column each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Flags {
    /// `future`.
    pub future: bool,
    /// `late`.
    pub late: bool,
    /// `revise`.
    pub revise: bool,
    /// `host_time`.
    pub host_time: bool,
    /// `unsynced_clock`.
    pub unsynced_clock: bool,
    /// Recorded without a value (spec §2.5).
    pub no_value: bool,
    /// `rounded`.
    pub rounded: bool,
}

impl Flags {
    /// The flags in [`column::FLAGS`](crate::column::FLAGS) order.
    pub fn to_array(self) -> [bool; 7] {
        [
            self.future,
            self.late,
            self.revise,
            self.host_time,
            self.unsynced_clock,
            self.no_value,
            self.rounded,
        ]
    }

    /// The flags from [`column::FLAGS`](crate::column::FLAGS) order.
    pub fn from_array(a: [bool; 7]) -> Self {
        let [
            future,
            late,
            revise,
            host_time,
            unsynced_clock,
            no_value,
            rounded,
        ] = a;
        Flags {
            future,
            late,
            revise,
            host_time,
            unsynced_clock,
            no_value,
            rounded,
        }
    }
}

impl From<FactFlags> for Flags {
    fn from(f: FactFlags) -> Self {
        Flags {
            future: f.future,
            late: f.late,
            revise: f.revise,
            host_time: f.host_time,
            unsynced_clock: f.unsynced_clock,
            no_value: f.no_value,
            rounded: f.rounded,
        }
    }
}

/// A fact as one export row (spec §2.2). A decimal value keeps the scale it was stored at.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportRow {
    /// Series.
    pub series: SeriesId,
    /// Event time.
    pub t: Timestamp,
    /// Position.
    pub position: Position,
    /// Host clock at commit.
    pub received_at: Timestamp,
    /// Value; [`Value::Absent`] when recorded without one.
    pub value: Value,
    /// Quality after stamping.
    pub quality: Quality,
    /// Quality as supplied.
    pub supplied_quality: Quality,
    /// Engine flags.
    pub flags: Flags,
    /// `changed_at`, in a versioned series.
    pub changed_at: Option<u64>,
}

impl From<&Fact> for ExportRow {
    fn from(f: &Fact) -> Self {
        ExportRow {
            series: f.series,
            t: f.t,
            position: f.position,
            received_at: f.received_at,
            value: f.value.clone(),
            quality: f.quality,
            supplied_quality: f.supplied_quality,
            flags: f.flags.into(),
            changed_at: f.changed_at,
        }
    }
}

impl From<Fact> for ExportRow {
    fn from(f: Fact) -> Self {
        ExportRow::from(&f)
    }
}
