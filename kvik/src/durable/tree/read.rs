//! The tree a reader takes with its cut (design 7.1b section 6): the newest root of the cut's generation and reset
//! stamp with K at or below P, its parts checked and kept while a later root lists them, and the stretch (K, P] of the
//! log, extended as P moves. Any doubt gives no tree, and the read takes facts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use kvik_log::writer::Position as LogPos;
use kvik_tree::part::Part;
use kvik_tree::read::{newest_root, read_part, without};
use kvik_tree::root::Root;
use kvik_tree::set::{Key, Seen, visible_of};

use super::stretch::{Stretch, dirty_from_log};
use crate::durable::lock;
use crate::durable::read::cut::Cut;
use crate::env::Env;

/// The longest stretch (K, P] a reader folds into its reads, in log positions: past it the reader takes facts
/// (section 6 `tree_stretch_max`, Q-2070: positions stand in for the 64 MiB of log).
pub(crate) const STRETCH_MAX: u64 = 1 << 20;

/// What the reader's tree reads have done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Cuts given a tree.
    pub(crate) served: u64,
    /// Parts that would not read or check, whose periods were then read from facts (`tree_damaged`).
    pub(crate) damaged: u64,
}

/// The tree at one cut.
#[derive(Debug)]
pub(crate) struct TreeAt {
    /// The root, less damaged parts.
    pub(crate) root: Root,
    live: Vec<Arc<Part>>,
    stretch: Arc<Stretch>,
}

impl TreeAt {
    /// `visible` of one series; `None` when the parts do not form one.
    pub(crate) fn visible(&self, series: u32) -> Option<BTreeMap<Key, Seen>> {
        let live: Vec<&Part> = self.live.iter().map(|p| &**p).collect();
        visible_of(&self.root, &live, u64::from(series)).ok()
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
    parts: BTreeMap<u64, Arc<Part>>,
    damaged: BTreeSet<u64>,
    /// The stretch `(k, upto]` last read.
    stretch: Option<(LogPos, LogPos, Arc<Stretch>)>,
    pub(crate) counts: Counts,
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
    let fs = &*env.fs;
    let mut c = lock(cache);
    let mut root = newest_root(fs, dir, usable)?;
    if p.sequence.saturating_sub(root.checkpoint.sequence) > STRETCH_MAX {
        return None;
    }
    let mut reloaded = false;
    let live = loop {
        if c.root.as_ref().is_none_or(|r| r.seq != root.seq) {
            let listed: BTreeSet<u64> = root.parts.iter().map(|r| r.file_id).collect();
            c.parts.retain(|f, _| listed.contains(f));
            c.damaged.retain(|f| listed.contains(f));
            c.root = Some(root.clone());
        }
        let mut failed = Vec::new();
        for r in &root.parts {
            if c.parts.contains_key(&r.file_id) || c.damaged.contains(&r.file_id) {
                continue;
            }
            match read_part(fs, dir, &root, r.file_id) {
                Ok(part) => {
                    c.parts.insert(r.file_id, Arc::new(part));
                }
                Err(_) => failed.push(r.file_id),
            }
        }
        if failed.is_empty() {
            break root
                .parts
                .iter()
                .filter_map(|r| c.parts.get(&r.file_id).cloned())
                .collect::<Vec<_>>();
        }
        // A part merged away and removed after the root was listed: the newer root no longer names it.
        if !reloaded {
            reloaded = true;
            if let Some(newer) = newest_root(fs, dir, usable)
                && newer.seq != root.seq
            {
                root = newer;
                continue;
            }
        }
        c.counts.damaged += failed.len() as u64;
        c.damaged.extend(failed);
    };
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
    let damaged: Vec<u64> = c.damaged.iter().copied().collect();
    c.counts.served += 1;
    Some(Arc::new(TreeAt {
        root: without(&root, &damaged),
        live,
        stretch,
    }))
}
