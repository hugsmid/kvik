//! The checkpoint job (design 7.1b section 5), stepped on `kvik-maint`: at its first step it starts the tree's files
//! and refills the dirty set from the log above K; then every interval, and at a clean shutdown, it brings each dirty
//! quarter-hour up to a K taken from a cut, writes a delta part and a root, and clears what it covered. Outside a
//! shutdown, at the same cut, it then closes open floors, rebuilds what the scrub found damaged, backfills and runs
//! the merges due (`maintain`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use kvik_log::manifest::LostRange;
use kvik_log::writer::Position as LogPos;
use kvik_model::summary::Params;
use kvik_tree::Level;
use kvik_tree::part::Entry;
use kvik_tree::set::{DAY, Policy, QUARTER};
use kvik_tree::write::{Fact, Identity, Opening, TreeFiles, folded_levels, quarter_entries};

use super::dirty::{Dirty, Key};
use super::stretch::dirty_from_log;
use crate::durable::index::Stored;
use crate::durable::loss::Latch;
use crate::durable::read::source::each_row;
use crate::durable::read::view::View;
use crate::durable::reader::DurableReader;
use crate::durable::reader::facts::plan;
use crate::durable::registry::Series;
use crate::durable::{lock, windows};
use crate::env::Env;
use crate::read::View as ReadView;
use crate::types::SeriesId;

mod maintain;

/// The checkpoint interval (Q-1423): taken this long after each quarter-hour of wall time.
pub(crate) const INTERVAL: Duration = Duration::from_secs(15 * 60);
const AFTER_QUARTER: Duration = Duration::from_secs(20);
/// How soon a failed job is tried again.
const RETRY: Duration = Duration::from_secs(60);
/// `tree_behind` once no checkpoint has succeeded for this many intervals (section 6 The stretch).
const BEHIND_AFTER: u32 = 3;
/// The merge planner's settings: a day's late window (Q-2061) and `tree.minute_keep` (Q-1420).
pub(crate) const POLICY: Policy = Policy {
    late_window: DAY,
    minute_keep: 35 * DAY,
};
/// How often the scrub reads every live part (section 7); also at the first step after a start.
const SCRUB: Duration = Duration::from_secs(86_400);
/// Backfill periods published per step; with more left the next step comes at once.
const BACKFILL_STEP: usize = 16;
/// Merges published per step.
const MERGES_STEP: usize = 16;

/// The store's reader, opened when `open` and not yet open (Q-1951); `Ok(None)` when not open and not to be opened.
pub(crate) type Readers =
    Box<dyn Fn(bool) -> Result<Option<Arc<DurableReader>>, String> + Send + Sync>;

/// See the module documentation.
pub(crate) struct Job {
    dir: PathBuf,
    env: Env,
    latch: Arc<Latch>,
    recorded: Vec<LostRange>,
    id: Identity,
    fresh: bool,
    dirty: Arc<Dirty>,
    readers: OnceLock<Readers>,
    interval: Duration,
    state: Mutex<State>,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct State {
    files: Option<TreeFiles>,
    /// When the next checkpoint is due, on the monotonic clock.
    due: Option<Duration>,
    first_step: Option<Duration>,
    last_step: Duration,
    last_ok: Option<Duration>,
    failed: Option<String>,
    /// When the scrub is next due; `None` until the first maintenance after a start.
    scrub_due: Option<Duration>,
}

/// What a job runs on besides the store's files.
pub(crate) struct Parts {
    pub(crate) dir: PathBuf,
    pub(crate) env: Env,
    pub(crate) latch: Arc<Latch>,
    pub(crate) recorded: Vec<LostRange>,
    pub(crate) id: Identity,
    /// The store held no series when it opened.
    pub(crate) fresh: bool,
    pub(crate) dirty: Arc<Dirty>,
}

impl Job {
    pub(crate) fn new(p: Parts) -> Self {
        Job {
            dir: p.dir,
            env: p.env,
            latch: p.latch,
            recorded: p.recorded,
            id: p.id,
            fresh: p.fresh,
            dirty: p.dirty,
            readers: OnceLock::new(),
            interval: INTERVAL,
            state: Mutex::new(State::default()),
        }
    }

    /// One step on `kvik-maint` with the mark synced through `synced`; returns how long until the next is due.
    pub(crate) fn step(&self, synced: LogPos, now: Duration, stopping: bool) -> Option<Duration> {
        let mut st = lock(&self.state);
        st.first_step.get_or_insert(now);
        st.last_step = now;
        if st.files.is_none() {
            if let Err(e) = self.start(&mut st, synced, now) {
                st.failed = Some(e);
                return (!stopping).then_some(RETRY);
            }
            st.failed = None;
            st.due = Some(now + self.until_due());
        }
        if !stopping && st.due.is_some_and(|d| d > now) {
            return st.due.map(|d| d - now);
        }
        match self.checkpoint(&mut st, synced, now, stopping) {
            Ok(None) => {}
            Ok(Some(more)) => {
                st.last_ok = Some(now);
                st.failed = None;
                st.due = Some(
                    now + if more {
                        Duration::ZERO
                    } else {
                        self.until_due()
                    },
                );
            }
            Err(e) => {
                st.failed = Some(e);
                st.due = Some(now + RETRY);
            }
        }
        st.due.map(|d| d.saturating_sub(now))
    }

    /// `tree_behind` (section 6): the cause of the last failed job, or how long no checkpoint has succeeded once that
    /// is longer than [`BEHIND_AFTER`] intervals, as of the last step.
    pub(crate) fn behind(&self) -> Option<String> {
        let st = lock(&self.state);
        let now = st.last_step;
        if let Some(e) = &st.failed {
            return Some(e.clone());
        }
        let since = st.last_ok.or(st.first_step)?;
        let gap = now.saturating_sub(since);
        (gap > self.interval * BEHIND_AFTER)
            .then(|| format!("no checkpoint for {} s", gap.as_secs()))
    }

    /// The wait until 20 s past the next boundary of the interval in wall time.
    fn until_due(&self) -> Duration {
        let wall = self.env.clock.wall().0;
        let step = i64::try_from(self.interval.as_nanos()).unwrap_or(i64::MAX);
        let after = i64::try_from(AFTER_QUARTER.as_nanos()).unwrap_or(0);
        let next = wall - (wall - after).rem_euclid(step) + step;
        Duration::from_nanos(u64::try_from(next - wall).unwrap_or(0))
    }

    /// Gives the job the store's reader (Q-1951).
    pub(crate) fn readers(&self, f: Readers) {
        let _ = self.readers.set(f);
    }

    /// Whether a loss this run found, that no lost range of the manifest held at the start, begins at or below `k`
    /// (Q-1429).
    fn lost_below(&self, k: LogPos) -> bool {
        crate::durable::derived::lost_since(&self.latch.found(), &self.recorded, k)
    }

    fn start(&self, st: &mut State, synced: LogPos, now: Duration) -> Result<(), String> {
        let at = Opening {
            mark: synced,
            fresh: self.fresh,
            wall: self.env.clock.wall().0,
            now,
        };
        let (files, _) = TreeFiles::start(self.env.fs.clone(), &self.dir, self.id, at, |k| {
            self.lost_below(k)
        })
        .map_err(|e| format!("tree start: {e}"))?;
        let s = dirty_from_log(&self.dir, &self.env, files.root().checkpoint, synced)
            .map_err(|e| format!("tree start: {e}"))?;
        self.dirty.fill(&s.keys, &s.series);
        st.files = Some(files);
        Ok(())
    }

    /// Whether a checkpoint was taken, and if so whether backfill is left for the next step: a clean shutdown takes
    /// one only on a reader already open, so that it never reads the log to build one (Q-1950), and does no more.
    fn checkpoint(
        &self,
        st: &mut State,
        synced: LogPos,
        now: Duration,
        stopping: bool,
    ) -> Result<Option<bool>, String> {
        let r = match self.readers.get() {
            Some(f) => f(!stopping).map_err(|e| format!("tree reader: {e}"))?,
            None => None,
        };
        let Some(r) = r else {
            return if stopping {
                Ok(None)
            } else {
                Err("tree reader: none".into())
            };
        };
        let v = r.snapshot().map_err(|e| format!("tree cut: {e}"))?;
        let out = self
            .checkpoint_at(st, &r, &v, synced, now)
            .and_then(|()| match stopping {
                true => Ok(false),
                false => self.maintain(st, &r, &v, now),
            });
        let _ = r.end(v);
        out.map(Some)
    }

    fn checkpoint_at(
        &self,
        st: &mut State,
        r: &DurableReader,
        v: &View,
        synced: LogPos,
        now: Duration,
    ) -> Result<(), String> {
        let k = v.cut().durable;
        if v.cut().stamp != self.id.reset {
            return Err("the mark was reset since the start".into());
        }
        // K is taken before the set: every fact at or below it was marked before it was offered.
        let (keys, rebuild) = self.dirty.take();
        let files = st.files.as_mut().expect("started");
        let wall = self.env.clock.wall().0;
        let root = files.root();
        if (!root.covered.is_empty() || !root.parts.is_empty()) && self.lost_below(root.checkpoint)
        {
            files.discard(wall, now).map_err(|e| e.to_string())?;
        }
        let root = files.root();
        let mut skip: BTreeSet<u64> = rebuild.iter().map(|&s| u64::from(s)).collect();
        skip.extend(
            root.floors
                .iter()
                .filter(|f| f.rebuilding)
                .map(|f| f.series),
        );
        let mut entries = entries(r, v, &keys, &skip, &root.levels.clone())?;
        let ups = files
            .ancestors(&entries, params_at(v))
            .map_err(|e| e.to_string())?;
        entries.extend(ups);
        let rebuilding: BTreeSet<u64> = rebuild.iter().map(|&s| u64::from(s)).collect();
        files
            .checkpoint(k, synced, entries, &rebuilding, wall, now)
            .map_err(|e| e.to_string())?;
        self.dirty.cleared(&keys, &rebuild, k);
        Ok(())
    }
}

/// The levels a series stores among the folded ones (spec §5.1 density rule, §5.4 for interval series).
fn stored(p: &Params, levels: &[Level]) -> Vec<Level> {
    folded_levels(levels)
        .into_iter()
        .filter(|l| match l.width {
            w if w == kvik_tree::MINUTE => p.stores(0),
            w if w == QUARTER => p.stores(1),
            _ => false,
        })
        .collect()
}

/// The entries of every dirty quarter-hour of a series that stores a level, from its current facts at `v`'s cut;
/// contiguous quarter-hours of one series are read as one range.
fn entries(
    r: &DurableReader,
    v: &View,
    keys: &[Key],
    skip: &BTreeSet<u64>,
    levels: &[Level],
) -> Result<BTreeMap<(u64, u8), Vec<Entry>>, String> {
    let mut out: BTreeMap<(u64, u8), Vec<Entry>> = BTreeMap::new();
    let registry = v.latest().registry();
    let mut i = 0;
    while i < keys.len() {
        let id = keys[i].0;
        let mut j = i;
        while j < keys.len() && keys[j].0 == id {
            j += 1;
        }
        let quarters: Vec<i64> = keys[i..j].iter().map(|k| k.1).collect();
        i = j;
        let Some(series) = registry.get(id) else {
            continue;
        };
        let p = windows::params(&series.def);
        let levels = stored(&p, levels);
        if levels.is_empty() || skip.contains(&u64::from(id)) {
            continue;
        }
        let np = crate::fold::node_params(&p);
        let plan = plan(r, v, &[SeriesId(id)], ReadView::Current).map_err(|e| e.to_string())?;
        let mut a = 0;
        while a < quarters.len() {
            let mut b = a + 1;
            while b < quarters.len() && quarters[b] == quarters[b - 1] + QUARTER {
                b += 1;
            }
            let (from, to) = (quarters[a], quarters[b - 1] + QUARTER);
            let facts = facts_of(r, v, &plan, series, from, to - 1)?;
            for &q in &quarters[a..b] {
                let lo = facts.partition_point(|f| f.t < q);
                let hi = facts.partition_point(|f| f.t < q + QUARTER);
                for (l, e) in quarter_entries(&np, &levels, q, &facts[lo..hi]) {
                    out.entry((u64::from(id), l)).or_default().push(e);
                }
            }
            a = b;
        }
    }
    Ok(out)
}

/// The current facts of one series with `t` in `[from, to]`, ascending, the latest position per `t`.
fn facts_of(
    r: &DurableReader,
    v: &View,
    plan: &crate::durable::read::source::Plan,
    series: &Series,
    from: i64,
    to: i64,
) -> Result<Vec<Fact>, String> {
    let mut rows: Vec<Stored> = Vec::new();
    each_row(r, v, plan, from, to, |s| rows.push(s)).map_err(|e| e.to_string())?;
    rows.sort_by_key(|s| (s.row.t, s.position));
    rows.dedup_by(|later, earlier| {
        let same = later.row.t == earlier.row.t;
        if same {
            std::mem::swap(later, earlier);
        }
        same
    });
    Ok(rows
        .iter()
        .map(|s| Fact {
            t: s.row.t,
            reading: windows::reading_of(series, s).map(|(_, v)| v),
            at: s.position,
        })
        .collect())
}

/// A series' node parameters by id, from the registry at `v`'s cut.
fn params_at(v: &View) -> impl Fn(u64) -> Option<kvik_tree::Params> + '_ {
    let registry = v.latest().registry();
    move |s| {
        let s = registry.get(u32::try_from(s).ok()?)?;
        Some(crate::fold::node_params(&windows::params(&s.def)))
    }
}
