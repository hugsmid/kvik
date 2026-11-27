//! Maintenance at a checkpoint's cut (design 7.1b section 5 Rebuild and Merges, section 7): open floors closed by a
//! rebuild of their series, damaged parts' periods rebuilt, backfill toward full `covered` (newest first), and the
//! merges due. Each root a rebuild publishes holds at most one settled period at the checkpoint's K (Q-2112), so a
//! stop loses at most the one in hand.

use std::time::Duration;

use kvik_tree::root::{Covered, Root};
use kvik_tree::set::{self, DAY, QUARTER, settled_period};
use kvik_tree::write::{Rebuild, TreeFiles, WriteError, backfill_cover, series_entries};

use super::{BACKFILL_STEP, Job, MERGES_STEP, POLICY, SCRUB, State, facts_of, params_at, stored};
use crate::durable::read::view::View;
use crate::durable::reader::DurableReader;
use crate::durable::reader::facts::plan;
use crate::durable::windows;
use crate::read::View as ReadView;
use crate::types::SeriesId;

fn err(e: WriteError) -> String {
    e.to_string()
}

/// What one rebuild reads and where it puts it.
struct Scope<'a> {
    root: &'a Root,
    /// `[from, to)`; `None` for no bound.
    from: Option<i64>,
    to: Option<i64>,
    /// One part for everything, or else one per settled period.
    period: Option<(i64, u64)>,
    /// What `covered` will be once published: entries outside it are not written.
    covered: Vec<Covered>,
    wall: i64,
}

impl Job {
    /// Runs after a checkpoint at `v`'s cut; returns whether backfill is left for the next step.
    pub(super) fn maintain(
        &self,
        st: &mut State,
        r: &DurableReader,
        v: &View,
        now: Duration,
    ) -> Result<bool, String> {
        let wall = self.env.clock.wall().0;
        let State {
            files, scrub_due, ..
        } = st;
        let files = files.as_mut().expect("started");
        let open: Vec<u64> = files
            .root()
            .floors
            .iter()
            .filter(|f| f.rebuilding)
            .map(|f| f.series)
            .collect();
        if !open.is_empty() {
            let root = files.root().clone();
            let scope = Scope {
                root: &root,
                from: None,
                to: None,
                period: None,
                covered: root.covered.clone(),
                wall,
            };
            let mut rb = rebuild_of(r, v, &scope, &open)?;
            rb.close = open;
            files
                .rebuild_newest_first(v.cut().durable, rb, |_| Vec::new(), wall, now)
                .map_err(err)?;
        }
        if scrub_due.is_none_or(|d| d <= now) {
            repair(files, r, v, wall, now)?;
            *scrub_due = Some(now + SCRUB);
        }
        let mut left = false;
        for i in 0..=BACKFILL_STEP {
            let Some(range) = backfill_next(files.root(), wall) else {
                break;
            };
            if i == BACKFILL_STEP {
                left = true;
                break;
            }
            backfill(files, r, v, range, wall, now)?;
        }
        match files.merge_due(wall, &POLICY, params_at(v), MERGES_STEP, wall, now) {
            Ok(_) => Ok(left),
            Err(WriteError::Damaged(_)) => repair(files, r, v, wall, now).map(|()| left),
            Err(e) => Err(err(e)),
        }
    }
}

/// The served series of the registry at `v`, by id.
fn served(v: &View, root: &Root) -> Vec<u64> {
    v.latest()
        .registry()
        .iter()
        .map(|s| u64::from(s.def.id))
        .filter(|&s| set::served(root, s))
        .collect()
}

/// The entries of `ids` over the scope, from their current facts at `v`'s cut (section 5 Rebuild).
fn rebuild_of(r: &DurableReader, v: &View, sc: &Scope<'_>, ids: &[u64]) -> Result<Rebuild, String> {
    let registry = v.latest().registry();
    let probe = Root {
        covered: sc.covered.clone(),
        ..sc.root.clone()
    };
    let keep = |l: u8, a: i64, b: i64| set::covered(&probe, l, a, b);
    let mut rb = Rebuild::default();
    for &id in ids {
        let Some(series) = u32::try_from(id).ok().and_then(|i| registry.get(i)) else {
            continue;
        };
        let p = windows::params(&series.def);
        let levels = stored(&p, &sc.root.levels);
        if levels.is_empty() {
            continue;
        }
        let np = crate::fold::node_params(&p);
        let plan =
            plan(r, v, &[SeriesId(series.def.id)], ReadView::Current).map_err(|e| e.to_string())?;
        let to = sc.to.map_or(i64::MAX, |t| t - 1);
        let facts = facts_of(r, v, &plan, series, sc.from.unwrap_or(i64::MIN), to)?;
        for (l, e) in series_entries(&sc.root.levels, &levels, &np, &facts, keep) {
            let period = match sc.period {
                Some(p) => p,
                None => settled_period(e.start, sc.wall, &POLICY)
                    .ok_or_else(|| format!("no period holds {}", e.start))?,
            };
            rb.add(period, id, l, e);
        }
    }
    Ok(rb)
}

/// Rebuilds the period of every part the scrub finds damaged, widened to the live parts it overlaps (section 7).
fn repair(
    files: &mut TreeFiles,
    r: &DurableReader,
    v: &View,
    wall: i64,
    now: Duration,
) -> Result<(), String> {
    for _ in 0..=files.root().parts.len() {
        let Some(&bad) = files.scrub().first() else {
            return Ok(());
        };
        let (period, retire) = files
            .damaged_period(bad)
            .ok_or_else(|| format!("part {bad:#x} is not live"))?;
        let root = files.root().clone();
        let end = period
            .0
            .saturating_add(i64::try_from(period.1).unwrap_or(i64::MAX));
        let scope = Scope {
            root: &root,
            from: Some(period.0),
            to: Some(end),
            period: Some(period),
            covered: root.covered.clone(),
            wall,
        };
        let mut rb = rebuild_of(r, v, &scope, &served(v, &root))?;
        rb.retire = retire;
        files.rebuild(v.cut().durable, rb, wall, now).map_err(err)?;
    }
    Ok(())
}

/// The quarter-hour level's id and full range.
fn base_range(root: &Root, wall: i64) -> Option<Covered> {
    let base = root.levels.iter().find(|l| l.width == QUARTER)?;
    backfill_cover(&root.levels, None, None, wall, &POLICY)
        .into_iter()
        .find(|c| c.level == base.id)
}

/// The next range a backfill rebuilds, newest first: from the start of today's UTC day upwards, then each settled
/// period below what is covered; `None` once the quarter-hour level covers everything.
fn backfill_next(root: &Root, wall: i64) -> Option<(Option<i64>, Option<i64>)> {
    let full = base_range(root, wall)?;
    if set::covered(root, full.level, full.from, full.to) {
        return None;
    }
    let top = root
        .covered
        .iter()
        .find(|c| c.level == full.level && c.to == full.to);
    let Some(top) = top else {
        return Some((Some(wall - wall.rem_euclid(DAY)), None));
    };
    let from = top.from;
    let below = settled_period(from - 1, wall, &POLICY)
        .filter(|&(s, l)| s.checked_add(l as i64) == Some(from))
        .map_or(from - DAY, |(s, _)| s);
    Some((Some(below), Some(from)))
}

/// One backfill rebuild of `[a, b)` that extends `covered`; a range with no fact ends the walk, taking everything
/// below it one settled period a root (Q-2062, Q-2112).
fn backfill(
    files: &mut TreeFiles,
    r: &DurableReader,
    v: &View,
    (a, b): (Option<i64>, Option<i64>),
    wall: i64,
    now: Duration,
) -> Result<(), String> {
    let root = files.root().clone();
    let ids = served(v, &root);
    let attempt = |from: Option<i64>| -> Result<(Rebuild, Vec<Covered>), String> {
        let cover = backfill_cover(&root.levels, from, b, wall, &POLICY);
        let scope = Scope {
            root: &root,
            from,
            to: b,
            period: None,
            covered: root.covered.iter().chain(&cover).copied().collect(),
            wall,
        };
        Ok((rebuild_of(r, v, &scope, &ids)?, cover))
    };
    let (mut rb, cover) = attempt(a)?;
    if rb.parts.is_empty() && a.is_some() {
        let (rb, _) = attempt(None)?;
        let cover = |from| backfill_cover(&root.levels, from, b, wall, &POLICY);
        return files
            .rebuild_newest_first(v.cut().durable, rb, cover, wall, now)
            .map_err(err);
    }
    rb.cover = cover;
    files.rebuild(v.cut().durable, rb, wall, now).map_err(err)
}
