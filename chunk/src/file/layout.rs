//! What a compacted file is said to be (its identity and the manifest's layout of it), and rows as read.

use kvik_log::entries::Entry;

use super::Position;
use super::desc::{Class, Description, Input, Level};
use super::parts::Trailer;
use super::write::Fact;

/// What a file's name and the store say it is: the store id, and the generation and `file_id` of its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Identity {
    /// The store's id (`format.md` A.12).
    pub store_id: [u8; 16],
    /// The generation the file was created in.
    pub generation: u32,
    /// Its `file_id`.
    pub file_id: u64,
}

/// What the manifest's publish record says about a compacted file beyond its file description (provisional:
/// `model/OPEN-QUESTIONS.md` Q-810). Everything here is in the file's description and trailer, so a rebuild
/// restates it from the file or its chunk index object.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Layout {
    /// The generation it was created in.
    pub generation: u32,
    /// Its `file_id`.
    pub file_id: u64,
    /// Its class.
    pub class: Class,
    /// Its level.
    pub level: Level,
    /// Its period's start.
    pub period_start: i64,
    /// Its period's length.
    pub period_len: i64,
    /// The run id.
    pub run_id: u64,
    /// The number of outputs published together.
    pub run_count: u32,
    /// Its inputs.
    pub inputs: Vec<Input>,
    /// The least and greatest position it holds.
    pub range: (Position, Position),
    /// The least `t` of a fact; 0 with none.
    pub min_t: i64,
    /// The greatest reach of a fact; 0 with none.
    pub max_reach: i64,
    /// The least commit time of its batch rows.
    pub first_commit_time: i64,
    /// The greatest commit time of its batch rows.
    pub last_commit_time: i64,
    /// Facts held.
    pub fact_count: u64,
    /// Positions held.
    pub entry_count: u64,
    /// It holds entries.
    pub has_entries: bool,
    /// It holds set-aside chunks.
    pub has_set_aside: bool,
    /// A split part's first and last series name.
    pub name_range: Option<(String, String)>,
}

impl Layout {
    pub(crate) fn of(id: &Identity, d: &Description, t: &Trailer) -> Self {
        Self {
            generation: id.generation,
            file_id: id.file_id,
            class: d.class,
            level: d.level,
            period_start: d.period_start,
            period_len: d.period_len,
            run_id: d.run_id,
            run_count: d.run_count,
            inputs: d.inputs.clone(),
            range: (t.first, t.last),
            min_t: t.min_t,
            max_reach: t.max_reach,
            first_commit_time: t.first_commit_time,
            last_commit_time: t.last_commit_time,
            fact_count: t.fact_count,
            entry_count: t.entry_count,
            has_entries: d.has_entries,
            has_set_aside: d.has_set_aside,
            name_range: d.name_range.clone(),
        }
    }

    /// The first field in which `self` and `other` differ.
    pub(crate) fn difference(&self, o: &Self) -> Option<&'static str> {
        [
            ("generation", self.generation == o.generation),
            ("file_id", self.file_id == o.file_id),
            ("class", self.class == o.class),
            ("level", self.level == o.level),
            (
                "period",
                (self.period_start, self.period_len) == (o.period_start, o.period_len),
            ),
            (
                "run",
                (self.run_id, self.run_count) == (o.run_id, o.run_count),
            ),
            ("inputs", self.inputs == o.inputs),
            ("position range", self.range == o.range),
            ("min_t", self.min_t == o.min_t),
            ("max_reach", self.max_reach == o.max_reach),
            (
                "commit times",
                (self.first_commit_time, self.last_commit_time)
                    == (o.first_commit_time, o.last_commit_time),
            ),
            ("fact_count", self.fact_count == o.fact_count),
            ("entry_count", self.entry_count == o.entry_count),
            (
                "flags",
                (self.has_entries, self.has_set_aside) == (o.has_entries, o.has_set_aside),
            ),
            ("name range", self.name_range == o.name_range),
        ]
        .into_iter()
        .find(|(_, same)| !same)
        .map(|(f, _)| f)
    }
}

/// A fact as read, with its batch's receive and commit times.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TimedFact {
    /// The fact.
    pub fact: Fact,
    /// Its batch's `received_at`.
    pub received_at: i64,
    /// Its batch's commit time.
    pub commit_time: i64,
}

/// A non-fact entry as read: its position, its batch's times and the entry. An [`Entry::Ignored`] stands for that
/// many positions from `position` on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntryRow {
    /// Its position.
    pub position: Position,
    /// Its batch's `received_at`.
    pub received_at: i64,
    /// Its batch's commit time.
    pub commit_time: i64,
    /// The entry.
    pub entry: Entry,
}
