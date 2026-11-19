//! Series labels (spec §2.1 Labels, §2.3 Label views): versioned facts about a
//! series, held as entries of type 10 in the series' log and folded over two
//! time axes, the effective time and the position.
//!
//! [`LabelLog`] holds the label entries of the kept log in position order. Every
//! view is a function of those entries and the registry built from the same
//! kept log; limits are checked by the calls only, never by [`LabelLog::replay`].

use std::collections::{BTreeMap, BTreeSet};

use crate::registry::Registry;
use crate::types::{Nanos, Position, SeriesId};

/// Longest key, in bytes (§2.1 Labels).
pub const MAX_KEY_LEN: usize = 63;
/// Longest value, in bytes (§2.1 Labels).
pub const MAX_VALUE_LEN: usize = 255;
/// Most items in one entry, and so in one change (`format.md` A.6 field 70).
pub const MAX_ITEMS: usize = 64;
/// Most distinct keys a series may ever use (§2.1 Labels).
pub const MAX_KEYS_PER_SERIES: usize = 64;
/// Keys refused by every call: the export layout's folder names (Q-881).
pub const RESERVED_KEYS: [&str; 6] = ["site", "area", "device", "metric", "month", "series"];
/// Prefix of the keys the engine reserves for itself (Q-881).
pub const RESERVED_PREFIX: &str = "kvik_";

/// When a label change takes effect (§2.1 Labels). `Always` sorts before
/// every time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Effective {
    /// Before every event time: an absent field 20 (`format.md` A.6 type 10).
    Always,
    /// From this event time on.
    At(Nanos),
}

/// One item of a label entry (`format.md` A.6 field 70 `op`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LabelItem {
    /// Op 1: the key holds this value from the entry's effective time.
    Set(String),
    /// Op 2: the key is absent from the entry's effective time.
    Remove,
    /// Op 3: the change at this key and effective time reads as never made.
    Withdraw,
}

/// A label entry of the log (entry type 10): one series, one effective time,
/// 1 to [`MAX_ITEMS`] items with distinct keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelEntry {
    /// Position of the entry.
    pub position: Position,
    /// Commit time of the entry's batch (§2.2).
    pub recorded_at: Nanos,
    /// Series.
    pub series: SeriesId,
    /// Effective time.
    pub from: Effective,
    /// Items in the order given.
    pub items: Vec<(String, LabelItem)>,
}

impl LabelEntry {
    /// The entry's encoded length with its header and padding (`format.md`
    /// A.6), the measure of `limits.label_bytes` (Q-1022).
    pub fn bytes(&self) -> u64 {
        let pad4 = |n: u64| n.div_ceil(4) * 4;
        let mut fields = 8;
        if matches!(self.from, Effective::At(_)) {
            fields += 12;
        }
        for (key, item) in &self.items {
            let value = match item {
                LabelItem::Set(v) => v.len() as u64,
                LabelItem::Remove | LabelItem::Withdraw => 0,
            };
            fields += 4 + pad4(2 + key.len() as u64 + value);
        }
        8 + fields.div_ceil(8) * 8
    }
}

/// A label version (§2.1 Labels): one item of one entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelVersion {
    /// Series.
    pub series: SeriesId,
    /// Key.
    pub key: String,
    /// Effective time.
    pub from: Effective,
    /// Position of its entry.
    pub position: Position,
    /// Commit time of its entry's batch.
    pub recorded_at: Nanos,
    /// The item.
    pub item: LabelItem,
    /// Whether a later version holds the same `(series, key, from)`.
    pub superseded: bool,
}

/// Which versions a label view takes (§2.3 Label views).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelView {
    /// Every version of the kept log.
    Current,
    /// The versions with position ≤ P.
    AsOf(Position),
    /// As of the last label position whose commit time is ≤ T.
    AsOfTime(Nanos),
}

/// The configurable label limits (§2.1 Labels, Q-880), checked at the call
/// only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelLimits {
    /// `limits.label_versions_per_series`.
    pub versions_per_series: u64,
    /// `limits.label_versions`.
    pub versions: u64,
    /// `limits.label_bytes`.
    pub bytes: u64,
}

impl Default for LabelLimits {
    fn default() -> Self {
        LabelLimits {
            versions_per_series: 1_024,
            versions: 1_000_000,
            bytes: 32 << 20,
        }
    }
}

impl LabelLimits {
    /// The documented defaults of the minimal class.
    pub const fn minimal() -> Self {
        LabelLimits {
            versions_per_series: 1_024,
            versions: 100_000,
            bytes: 4 << 20,
        }
    }
}

/// Which store-wide label limit a call would pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreLimit {
    /// `limits.label_versions`.
    Versions,
    /// `limits.label_bytes`.
    Bytes,
}

/// Why a label call is refused; a refused call appends nothing (`api.md`
/// `LabelError`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LabelError {
    /// No definition of the series in the kept log.
    #[error("unknown series {0}")]
    UnknownSeries(SeriesId),
    /// A key outside `[a-z][a-z0-9_]*` of 1 to 63 bytes.
    #[error("bad label key {key:?}")]
    BadKey {
        /// The key.
        key: String,
    },
    /// A reserved key (Q-881).
    #[error("label key {key:?} is reserved")]
    Reserved {
        /// The key.
        key: String,
    },
    /// A value empty, over 255 bytes or holding a control character.
    #[error("bad value for label key {key:?}")]
    BadValue {
        /// The key.
        key: String,
    },
    /// A key twice in one change.
    #[error("label key {key:?} given twice in one change")]
    Duplicate {
        /// The key.
        key: String,
    },
    /// More than [`MAX_ITEMS`] items in one change.
    #[error("more than {MAX_ITEMS} items in one label change")]
    TooManyItems,
    /// The series would use more than [`MAX_KEYS_PER_SERIES`] distinct keys.
    #[error("series would use more than {limit} label keys")]
    TooManyKeys {
        /// The limit.
        limit: usize,
    },
    /// The series would hold more versions than its limit.
    #[error("series would hold more than {limit} label versions")]
    SeriesLimit {
        /// The limit.
        limit: u64,
    },
    /// The store would pass a store-wide limit.
    #[error("store label limit {which:?} of {limit} reached")]
    StoreLimit {
        /// Which limit.
        which: StoreLimit,
        /// The limit.
        limit: u64,
    },
}

/// A change to some keys of one series at one effective time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabelChange {
    items: Vec<(String, LabelItem)>,
}

impl LabelChange {
    /// An empty change.
    pub fn new() -> Self {
        LabelChange::default()
    }

    /// Adds a set.
    pub fn set(mut self, key: &str, value: &str) -> Self {
        self.items
            .push((key.to_string(), LabelItem::Set(value.to_string())));
        self
    }

    /// Adds a removal.
    pub fn remove(mut self, key: &str) -> Self {
        self.items.push((key.to_string(), LabelItem::Remove));
        self
    }

    /// Adds a withdrawal.
    pub fn withdraw(mut self, key: &str) -> Self {
        self.items.push((key.to_string(), LabelItem::Withdraw));
        self
    }

    /// Adds an item.
    pub fn item(mut self, key: &str, item: LabelItem) -> Self {
        self.items.push((key.to_string(), item));
        self
    }

    /// The items in the order given.
    pub fn items(&self) -> &[(String, LabelItem)] {
        &self.items
    }
}

/// What a label call recorded (`api.md` `LabelOutcome`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelOutcome {
    /// Position of the last entry appended; `None` when nothing was (Q-1021).
    pub position: Option<Position>,
    /// Items recorded.
    pub recorded: u32,
    /// Items dropped by the duplicate rule.
    pub duplicates: u32,
}

/// Label limits at or above 80 %, for health (§2.1 Labels).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabelNotices {
    /// Store-wide limits at 80 % or more.
    pub store: Vec<StoreLimit>,
    /// Series at 80 % or more of `limits.label_versions_per_series`.
    pub series: Vec<SeriesId>,
}

/// Whether `key` follows the key grammar (§2.1 Labels).
pub fn key_is_valid(key: &str) -> bool {
    let b = key.as_bytes();
    (1..=MAX_KEY_LEN).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// Whether `key` is reserved (Q-881).
pub fn key_is_reserved(key: &str) -> bool {
    RESERVED_KEYS.contains(&key) || key.starts_with(RESERVED_PREFIX)
}

/// Whether `value` follows the value rule (§2.1 Labels).
pub fn value_is_valid(value: &str) -> bool {
    (1..=MAX_VALUE_LEN).contains(&value.len())
        && !value.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}')
}

/// Checks one change's items in order: grammar, reservation, a key once, the
/// item count (Q-1023 orders the checks).
pub fn check_items(items: &[(String, LabelItem)]) -> Result<(), LabelError> {
    let mut seen = BTreeSet::new();
    for (key, item) in items {
        if !key_is_valid(key) {
            return Err(LabelError::BadKey { key: key.clone() });
        }
        if key_is_reserved(key) {
            return Err(LabelError::Reserved { key: key.clone() });
        }
        if let LabelItem::Set(v) = item
            && !value_is_valid(v)
        {
            return Err(LabelError::BadValue { key: key.clone() });
        }
        if !seen.insert(key.as_str()) {
            return Err(LabelError::Duplicate { key: key.clone() });
        }
    }
    if items.len() > MAX_ITEMS {
        return Err(LabelError::TooManyItems);
    }
    Ok(())
}

/// Where a call's batch goes: its first position and its commit time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Batch {
    /// Position of the batch's first entry.
    pub first: Position,
    /// Commit time of the batch.
    pub recorded_at: Nanos,
}

/// The label entries of the kept log, in position order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabelLog {
    entries: Vec<LabelEntry>,
}

impl LabelLog {
    /// An empty log.
    pub fn new() -> Self {
        LabelLog::default()
    }

    /// Replays kept entries: every entry applies whatever the limits now
    /// configured (§2.1 Labels: limits at the call only).
    pub fn replay(entries: impl IntoIterator<Item = LabelEntry>) -> Self {
        let mut log = LabelLog::new();
        for e in entries {
            log.apply(e);
        }
        log
    }

    /// Applies one kept entry.
    ///
    /// # Panics
    /// Panics if the entry's position is not above the last one's.
    pub fn apply(&mut self, e: LabelEntry) {
        if let Some(last) = self.entries.last() {
            assert!(
                e.position > last.position,
                "label entries in position order"
            );
        }
        self.entries.push(e);
    }

    /// The entries in position order.
    pub fn entries(&self) -> &[LabelEntry] {
        &self.entries
    }

    /// The entries with position ≤ `p`, as a restore to `p` keeps them (§8).
    pub fn cut(&self, p: Position) -> LabelLog {
        LabelLog {
            entries: self
                .entries
                .iter()
                .filter(|e| e.position <= p)
                .cloned()
                .collect(),
        }
    }

    /// Label versions held, orphaned entries included.
    pub fn versions(&self) -> u64 {
        self.entries.iter().map(|e| e.items.len() as u64).sum()
    }

    /// Bytes of all label entries held (Q-1022).
    pub fn bytes(&self) -> u64 {
        self.entries.iter().map(LabelEntry::bytes).sum()
    }

    /// Entries whose series has no definition in the kept log
    /// (`orphaned_label_entries`, §2.1 Labels).
    pub fn orphaned_entries(&self, reg: &Registry) -> usize {
        self.entries
            .iter()
            .filter(|e| reg.get(e.series).is_none())
            .count()
    }

    fn known_at(&self, view: LabelView) -> Position {
        match view {
            LabelView::Current => self.entries.last().map_or(Position::ORIGIN, |e| e.position),
            LabelView::AsOf(p) => p,
            LabelView::AsOfTime(t) => self
                .entries
                .iter()
                .take_while(|e| e.recorded_at <= t)
                .last()
                .map_or(Position::ORIGIN, |e| e.position),
        }
    }

    /// The current item of each `(key, from)` of a series among the versions
    /// known at `p`: the one of highest position (§2.3, the fact rule).
    fn current(&self, series: SeriesId, p: Position) -> BTreeMap<(&str, Effective), &LabelItem> {
        let mut out = BTreeMap::new();
        for e in self
            .entries
            .iter()
            .filter(|e| e.series == series && e.position <= p)
        {
            for (key, item) in &e.items {
                out.insert((key.as_str(), e.from), item);
            }
        }
        out
    }

    /// The labels in force on `series` at event time `t` under `view` (§2.3
    /// Label views); empty for a series with no definition in `reg`.
    pub fn labels_at(
        &self,
        reg: &Registry,
        series: SeriesId,
        t: Nanos,
        view: LabelView,
    ) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if reg.get(series).is_none() {
            return out;
        }
        let mut latest: BTreeMap<&str, (Effective, &LabelItem)> = BTreeMap::new();
        for ((key, from), item) in self.current(series, self.known_at(view)) {
            if from > Effective::At(t) || *item == LabelItem::Withdraw {
                continue;
            }
            if latest.get(key).is_none_or(|(f, _)| from > *f) {
                latest.insert(key, (from, item));
            }
        }
        for (key, (_, item)) in latest {
            if let LabelItem::Set(v) = item {
                out.insert(key.to_string(), v.clone());
            }
        }
        out
    }

    /// The labels now: in force at `now`, every version known.
    pub fn labels(&self, reg: &Registry, series: SeriesId, now: Nanos) -> BTreeMap<String, String> {
        self.labels_at(reg, series, now, LabelView::Current)
    }

    /// Every version of a series in position order, each marked current or
    /// superseded; empty for a series with no definition in `reg`.
    pub fn history(&self, reg: &Registry, series: SeriesId) -> Vec<LabelVersion> {
        if reg.get(series).is_none() {
            return Vec::new();
        }
        let mut out: Vec<LabelVersion> = Vec::new();
        let mut last: BTreeMap<(String, Effective), usize> = BTreeMap::new();
        for e in self.entries.iter().filter(|e| e.series == series) {
            for (key, item) in &e.items {
                if let Some(&i) = last.get(&(key.clone(), e.from)) {
                    out[i].superseded = true;
                }
                last.insert((key.clone(), e.from), out.len());
                out.push(LabelVersion {
                    series,
                    key: key.clone(),
                    from: e.from,
                    position: e.position,
                    recorded_at: e.recorded_at,
                    item: item.clone(),
                    superseded: false,
                });
            }
        }
        out
    }

    /// Each key carried now by some defined series, with the count of those
    /// series (`label_keys`).
    pub fn label_keys(&self, reg: &Registry, now: Nanos) -> BTreeMap<String, u32> {
        let mut out = BTreeMap::new();
        for s in self.labelled(reg) {
            for key in self.labels(reg, s, now).into_keys() {
                *out.entry(key).or_insert(0) += 1;
            }
        }
        out
    }

    /// Each value of `key` carried now, with the count of series
    /// (`label_values`).
    pub fn label_values(&self, reg: &Registry, key: &str, now: Nanos) -> BTreeMap<String, u32> {
        let mut out = BTreeMap::new();
        for s in self.labelled(reg) {
            if let Some(v) = self.labels(reg, s, now).remove(key) {
                *out.entry(v).or_insert(0) += 1;
            }
        }
        out
    }

    fn labelled(&self, reg: &Registry) -> BTreeSet<SeriesId> {
        self.entries
            .iter()
            .map(|e| e.series)
            .filter(|s| reg.get(*s).is_some())
            .collect()
    }

    /// The health notices of the label limits (§2.1 Labels): each store
    /// limit at 80 % or more, and each defined series at 80 % or more of its
    /// own. A start reports them; it never refuses its own log.
    pub fn notices(&self, reg: &Registry, limits: &LabelLimits) -> LabelNotices {
        let near = |used: u64, limit: u64| u128::from(used) * 5 >= u128::from(limit) * 4;
        let mut store = Vec::new();
        if near(self.versions(), limits.versions) {
            store.push(StoreLimit::Versions);
        }
        if near(self.bytes(), limits.bytes) {
            store.push(StoreLimit::Bytes);
        }
        let series = self
            .labelled(reg)
            .into_iter()
            .filter(|s| near(self.series_versions(*s), limits.versions_per_series))
            .collect();
        LabelNotices { store, series }
    }

    fn series_versions(&self, series: SeriesId) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.series == series)
            .map(|e| e.items.len() as u64)
            .sum()
    }

    fn series_keys(&self, series: SeriesId) -> usize {
        self.entries
            .iter()
            .filter(|e| e.series == series)
            .flat_map(|e| e.items.iter().map(|(k, _)| k.as_str()))
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// Records several changes in one batch, the entries at consecutive
    /// positions from `batch.first` (`set_labels_many`). One bad item, or a limit
    /// passed by the versions the call adds, refuses the whole call (Q-1024).
    /// An item is dropped only when the current version at exactly its
    /// `(series, key, from)`, earlier entries of the batch included, is
    /// already that item (§2.1 Labels, Duplicates).
    pub fn set_labels_many(
        &mut self,
        reg: &Registry,
        limits: &LabelLimits,
        changes: &[(SeriesId, Effective, LabelChange)],
        batch: Batch,
    ) -> Result<LabelOutcome, LabelError> {
        for (series, _, change) in changes {
            if reg.get(*series).is_none() {
                return Err(LabelError::UnknownSeries(*series));
            }
            check_items(change.items())?;
        }
        let mut next = self.clone();
        let mut outcome = LabelOutcome {
            position: None,
            recorded: 0,
            duplicates: 0,
        };
        let mut position = batch.first;
        for (series, from, change) in changes {
            let known = next.known_at(LabelView::Current);
            let current = next.current(*series, known);
            let items: Vec<(String, LabelItem)> = change
                .items()
                .iter()
                .filter(|(key, item)| current.get(&(key.as_str(), *from)) != Some(&item))
                .cloned()
                .collect();
            outcome.duplicates += (change.items().len() - items.len()) as u32;
            if items.is_empty() {
                continue;
            }
            outcome.recorded += items.len() as u32;
            outcome.position = Some(position);
            next.apply(LabelEntry {
                position,
                recorded_at: batch.recorded_at,
                series: *series,
                from: *from,
                items,
            });
            position = position.next();
        }
        let grown: BTreeSet<SeriesId> = next.entries[self.entries.len()..]
            .iter()
            .map(|e| e.series)
            .collect();
        for &s in &grown {
            if next.series_keys(s) > MAX_KEYS_PER_SERIES {
                return Err(LabelError::TooManyKeys {
                    limit: MAX_KEYS_PER_SERIES,
                });
            }
            if next.series_versions(s) > limits.versions_per_series {
                return Err(LabelError::SeriesLimit {
                    limit: limits.versions_per_series,
                });
            }
        }
        if !grown.is_empty() && next.versions() > limits.versions {
            return Err(LabelError::StoreLimit {
                which: StoreLimit::Versions,
                limit: limits.versions,
            });
        }
        if !grown.is_empty() && next.bytes() > limits.bytes {
            return Err(LabelError::StoreLimit {
                which: StoreLimit::Bytes,
                limit: limits.bytes,
            });
        }
        *self = next;
        Ok(outcome)
    }

    /// Records one change (`set_labels`).
    pub fn set_labels(
        &mut self,
        reg: &Registry,
        limits: &LabelLimits,
        series: SeriesId,
        from: Effective,
        change: LabelChange,
        batch: Batch,
    ) -> Result<LabelOutcome, LabelError> {
        self.set_labels_many(reg, limits, &[(series, from, change)], batch)
    }

    /// The start-up form (`declare_labels`, §2.1 Labels): sets from `now` each
    /// key whose value in force now differs; a key left out is left alone.
    pub fn declare(
        &mut self,
        reg: &Registry,
        limits: &LabelLimits,
        series: SeriesId,
        labels: &[(&str, &str)],
        now: Nanos,
        batch: Batch,
    ) -> Result<LabelOutcome, LabelError> {
        if reg.get(series).is_none() {
            return Err(LabelError::UnknownSeries(series));
        }
        let asked: Vec<(String, LabelItem)> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), LabelItem::Set(v.to_string())))
            .collect();
        check_items(&asked)?;
        let now_labels = self.labels(reg, series, now);
        let change = labels
            .iter()
            .filter(|(k, v)| now_labels.get(*k).map(String::as_str) != Some(*v))
            .fold(LabelChange::new(), |c, (k, v)| c.set(k, v));
        let in_force = (labels.len() - change.items().len()) as u32;
        let mut outcome =
            self.set_labels(reg, limits, series, Effective::At(now), change, batch)?;
        outcome.duplicates += in_force;
        Ok(outcome)
    }
}
