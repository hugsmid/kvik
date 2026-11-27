//! The tree a reader takes with its cut (design 7.1b section 6): the newest root of the cut's generation and reset
//! stamp with K at or below P, the stretch (K, P] of the log, extended as P moves, and the parts a read's span needs,
//! read on first use, checked against the root and kept while a later root lists them. Any doubt gives no tree, or
//! facts for a part's period, and the read takes facts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kvik_log::fs::Fs;
use kvik_log::writer::Position as LogPos;
use kvik_tree::part::Part;
use kvik_tree::read::{newest_root, read_part, within, without};
use kvik_tree::root::Root;
use kvik_tree::set::DAY;

use super::stretch::{Stretch, dirty_from_log};
use crate::durable::lock;
use crate::durable::read::cut::Cut;
use crate::env::Env;

/// The longest stretch (K, P] a reader folds into its reads, in log positions: past it the reader takes facts
/// (section 6 `tree_stretch_max`, Q-2070: positions stand in for the 64 MiB of log).
pub(crate) const STRETCH_MAX: u64 = 1 << 20;

/// What the reader's tree reads have done.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Cuts given a tree.
    pub(crate) served: u64,
    /// Parts that would not read or check, whose periods were then read from facts (`tree_damaged`).
    pub(crate) damaged: u64,
}

/// The parts read so far, by `file_id`, and those that failed.
#[derive(Debug, Default)]
pub(crate) struct Parts {
    read: BTreeMap<u64, Arc<Part>>,
    damaged: BTreeSet<u64>,
    found_damaged: u64,
}

/// The tree at one cut.
#[derive(Debug)]
pub(crate) struct TreeAt {
    root: Root,
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    parts: Arc<Mutex<Parts>>,
    stretch: Arc<Stretch>,
}

/// What the tree says of one span: the root covering only that span, less damaged parts, and the parts it needs.
#[derive(Debug)]
pub(crate) struct Window {
    pub(crate) root: Root,
    pub(crate) live: Vec<Arc<Part>>,
}

impl TreeAt {
    /// The tree over the UTC days that hold `[from, to)`; from the first instant when `from` is `i64::MIN`.
    pub(crate) fn window(&self, from: i64, to: i64) -> Window {
        let from = match from {
            i64::MIN => i64::MIN,
            f => f - f.rem_euclid(DAY),
        };
        let to = to.saturating_add(DAY - 1);
        let to = to - to.rem_euclid(DAY);
        let mut p = lock(&self.parts);
        let mut live = Vec::new();
        let mut damaged = Vec::new();
        for r in &self.root.parts {
            let end = r.period_start.saturating_add(r.period_len as i64);
            if end <= from || to <= r.period_start {
                continue;
            }
            if let Some(part) = p.read.get(&r.file_id) {
                live.push(part.clone());
                continue;
            }
            if !p.damaged.contains(&r.file_id) {
                match read_part(&*self.fs, &self.dir, &self.root, r.file_id) {
                    Ok(part) => {
                        let part = Arc::new(part);
                        p.read.insert(r.file_id, part.clone());
                        live.push(part);
                        continue;
                    }
                    Err(_) => {
                        p.damaged.insert(r.file_id);
                        p.found_damaged += 1;
                    }
                }
            }
            damaged.push(r.file_id);
        }
        Window {
            root: within(&without(&self.root, &damaged), from, to),
            live,
        }
    }

    /// Whether a fact in the stretch touches a quarter-hour of `series` in `[from, to)`.
    pub(crate) fn dirty(&self, series: u32, from: i64, to: i64) -> bool {
        from < to
            && self
                .stretch
                .keys
                .range((series, from)..(series, to))
                .next()
                .is_some()
    }

    /// Whether a node-changing entry in the stretch names `series`.
    pub(crate) fn changed(&self, series: u32) -> bool {
        self.stretch.series.contains(&series)
    }
}

/// A reader's tree state across cuts.
#[derive(Debug, Default)]
pub(crate) struct Cache {
    /// The last root used, as on disk.
    root: Option<Root>,
    parts: Arc<Mutex<Parts>>,
    /// The stretch `(k, upto]` last read.
    stretch: Option<(LogPos, LogPos, Arc<Stretch>)>,
    #[cfg_attr(not(test), allow(dead_code))]
    served: u64,
}

impl Cache {
    #[cfg(test)]
    pub(crate) fn counts(&self) -> Counts {
        Counts {
            served: self.served,
            damaged: lock(&self.parts).found_damaged,
        }
    }
}

/// The tree for `cut`, if one may serve it.
pub(crate) fn at_cut(
    cache: &Mutex<Cache>,
    dir: &Path,
    env: &Env,
    store_id: [u8; 16],
    cut: &Cut,
) -> Option<Arc<TreeAt>> {
    let p = cut.durable;
    let usable = |r: &Root| {
        r.header.generation == p.generation
            && r.header.store_id == store_id
            && r.reset == cut.stamp
            && r.checkpoint <= p
            && r.checkpoint.generation == p.generation
    };
    let mut c = lock(cache);
    let root = newest_root(&*env.fs, dir, usable)?;
    if p.sequence.saturating_sub(root.checkpoint.sequence) > STRETCH_MAX {
        return None;
    }
    if c.root.as_ref().is_none_or(|r| r.seq != root.seq) {
        let listed: BTreeSet<u64> = root.parts.iter().map(|r| r.file_id).collect();
        let mut parts = lock(&c.parts);
        parts.read.retain(|f, _| listed.contains(f));
        parts.damaged.retain(|f| listed.contains(f));
        drop(parts);
        c.root = Some(root.clone());
    }
    let k = root.checkpoint;
    let stretch = match c.stretch.take() {
        Some((ck, upto, s)) if ck == k && upto == p => s,
        Some((ck, upto, s)) if ck == k && upto < p => {
            let more = dirty_from_log(dir, env, upto, p).ok()?;
            let mut s = Arc::unwrap_or_clone(s);
            for (key, at) in more.keys {
                let e = s.keys.entry(key).or_default();
                *e = (*e).max(at);
            }
            s.series.extend(more.series);
            Arc::new(s)
        }
        _ => Arc::new(dirty_from_log(dir, env, k, p).ok()?),
    };
    c.stretch = Some((k, p, stretch.clone()));
    c.served += 1;
    Some(Arc::new(TreeAt {
        root,
        fs: env.fs.clone(),
        dir: dir.to_path_buf(),
        parts: c.parts.clone(),
        stretch,
    }))
}
