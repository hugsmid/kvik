//! The catalogue's rule for compacted files (compaction design §3.4, invariant 14 in code), judged on the
//! catalogue's state whatever order its records came in, and C(g), the end of the compacted prefix of generation g.
//!
//! The writer judges a new publish record's shape ([`publish_shape`]) and the state it would make ([`judge`]) and
//! refuses a breach or an undecided state (Q-2121); a start judges the replayed state and, when a rule cannot be
//! decided or fails, holds runs, merges, removals and releases instead of refusing the manifest. Rule 3 for a
//! replacement and rule 5 are not built: replacements and releases come with later items.

use std::collections::{BTreeMap, BTreeSet};

use crate::catalogue::{Catalogue, FileEntry};
use crate::desc::{FileRef, kind};
use crate::layout::CompactedLayout;
use crate::record::Publish;

/// A rule of compaction design §3.4 that the state breaks.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("compaction rule {rule} broken at {file:?}: {what}")]
pub struct Breach {
    /// The rule's number in §3.4.
    pub rule: u8,
    /// A file the breach names.
    pub file: FileRef,
    /// What is wrong.
    pub what: &'static str,
}

/// What the catalogue's state is under the rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Judgement {
    /// Rules 1 to 4 hold.
    Holds,
    /// A rule cannot be decided: a file whose record or layout the catalogue lacks. Names those files.
    Undecided(BTreeSet<FileRef>),
    /// A rule fails.
    Broken(Breach),
}

impl Judgement {
    /// Whether runs, merges, removals and releases are held (§3.4: undecided or broken at the end of a replay).
    pub fn holds_compaction(&self) -> bool {
        !matches!(self, Judgement::Holds)
    }
}

struct State<'a> {
    c: &'a Catalogue,
    unknown: BTreeSet<FileRef>,
}

impl State<'_> {
    fn layout(&mut self, f: FileRef) -> Option<&CompactedLayout> {
        let l = self.c.layout(f);
        if l.is_none() {
            self.unknown.insert(f);
        }
        l
    }

    fn segment_range(&mut self, f: FileRef) -> Option<((u32, u64), (u32, u64))> {
        let r = self
            .c
            .file(f)
            .filter(|e| e.description.kind == kind::LOG_SEGMENT)
            .and_then(|e| e.description.range);
        if r.is_none() {
            self.unknown.insert(f);
        }
        r
    }
}

fn breach(rule: u8, file: FileRef, what: &'static str) -> Judgement {
    Judgement::Broken(Breach { rule, file, what })
}

/// The compacted files the rule covers: those a record gave a layout. A record without field 3 comes from before it
/// and is outside the rule (Q-1850).
fn compacted(c: &Catalogue) -> impl Iterator<Item = &FileEntry> {
    c.files().filter(|e| {
        e.description.kind == kind::COMPACTED && c.layout(e.description.file_ref()).is_some()
    })
}

/// Judges the catalogue's state under rules 1 to 4 of §3.4.
pub fn judge(c: &Catalogue) -> Judgement {
    let mut s = State {
        c,
        unknown: BTreeSet::new(),
    };
    // Rule 4, shape of a file's inputs; rule 1, one run per segment.
    let mut runs_of: BTreeMap<FileRef, BTreeSet<u64>> = BTreeMap::new();
    for e in compacted(c) {
        let d = &e.description;
        let logs = d
            .inputs
            .iter()
            .filter(|i| i.kind == kind::LOG_SEGMENT)
            .count();
        if logs != 0 && logs != d.inputs.len() {
            return breach(
                4,
                d.file_ref(),
                "inputs both log segments and compacted files",
            );
        }
        if logs == 0 {
            continue;
        }
        let Some(run) = c.layout(d.file_ref()).map(|l| l.run_id) else {
            continue;
        };
        for i in &d.inputs {
            runs_of.entry(*i).or_default().insert(run);
        }
    }
    for (seg, runs) in &runs_of {
        if runs.len() > 1 {
            return breach(1, *seg, "a log segment named by the files of two runs");
        }
    }
    if let Err(j) = prefixes(&mut s) {
        return j;
    }
    // Rule 3: a run's outputs lie inside the span of its inputs; a merge conserves counts.
    for e in compacted(c) {
        let d = &e.description;
        if d.inputs
            .first()
            .is_some_and(|i| i.kind == kind::LOG_SEGMENT)
        {
            let spans: Vec<_> = d.inputs.iter().map(|i| s.segment_range(*i)).collect();
            let Some(spans) = spans.into_iter().collect::<Option<Vec<_>>>() else {
                continue;
            };
            let lo = spans.iter().map(|r| r.0).min();
            let hi = spans.iter().map(|r| r.1).max();
            if d.range
                .zip(lo.zip(hi))
                .is_none_or(|((a, b), (lo, hi))| a < lo || b > hi)
            {
                return breach(
                    3,
                    d.file_ref(),
                    "positions outside the span of its input segments",
                );
            }
        }
    }
    if let Err(j) = runs(&mut s) {
        return j;
    }
    if s.unknown.is_empty() {
        Judgement::Holds
    } else {
        Judgement::Undecided(s.unknown)
    }
}

/// The listed log segments of each generation, each with its first and last sequence, in position order.
fn segments(c: &Catalogue) -> BTreeMap<u32, Vec<(u64, u64, FileRef)>> {
    let mut by_gen: BTreeMap<u32, Vec<(u64, u64, FileRef)>> = BTreeMap::new();
    for e in c
        .files()
        .filter(|e| e.description.kind == kind::LOG_SEGMENT)
    {
        if let Some(((g, a), (g2, b))) = e.description.range
            && g == g2
        {
            by_gen
                .entry(g)
                .or_default()
                .push((a, b, e.description.file_ref()));
        }
    }
    for segs in by_gen.values_mut() {
        segs.sort();
    }
    by_gen
}

/// The end of the named prefix of one generation's segments, and the first segment skipped below a named one. A
/// segment inside a named one's range counts as named (§3.4 rule 2: a copy that replaced it).
fn prefix(
    segs: &[(u64, u64, FileRef)],
    named: &BTreeSet<FileRef>,
) -> (Option<u64>, Option<FileRef>) {
    let spans: Vec<(u64, u64)> = segs
        .iter()
        .filter(|x| named.contains(&x.2))
        .map(|x| (x.0, x.1))
        .collect();
    let mut end = None;
    let mut gap = None;
    for (a, b, f) in segs {
        if named.contains(f) || spans.iter().any(|(x, y)| x <= a && b <= y) {
            if gap.is_some() {
                return (end, gap);
            }
            end = Some(end.map_or(*b, |e: u64| e.max(*b)));
        } else {
            gap.get_or_insert(*f);
        }
    }
    (end, None)
}

/// The log segments compacted files name as inputs, with a layout or without: a file a restated retirement names
/// has none, yet its inputs are compacted.
fn named_segments(c: &Catalogue) -> BTreeSet<FileRef> {
    c.files()
        .filter(|e| e.description.kind == kind::COMPACTED)
        .flat_map(|e| e.description.inputs.iter().copied())
        .filter(|i| i.kind == kind::LOG_SEGMENT)
        .collect()
}

/// Rule 2: per generation, the named segments are a prefix of the listed ones.
fn prefixes(s: &mut State<'_>) -> Result<(), Judgement> {
    let named = named_segments(s.c);
    for seg in &named {
        s.segment_range(*seg);
    }
    for segs in segments(s.c).values() {
        if let (_, Some(skipped)) = prefix(segs, &named) {
            return Err(breach(
                2,
                skipped,
                "a listed segment skipped below a compacted one",
            ));
        }
    }
    Ok(())
}

/// Rules 3 and 4 over each run id: as many files as its count, a merge's counts equal to its inputs'.
fn runs(s: &mut State<'_>) -> Result<(), Judgement> {
    let mut by_run: BTreeMap<u64, Vec<(FileRef, CompactedLayout, Vec<FileRef>)>> = BTreeMap::new();
    for e in compacted(s.c) {
        let d = &e.description;
        if let Some(l) = s.c.layout(d.file_ref()) {
            by_run
                .entry(l.run_id)
                .or_default()
                .push((d.file_ref(), l.clone(), d.inputs.clone()));
        }
    }
    // Any file without a layout may be a missing file of any short run, so a short run is undecided rather than
    // broken; either verdict holds compaction.
    let unlaid = s.c.files().any(|e| {
        e.description.kind == kind::COMPACTED && s.c.layout(e.description.file_ref()).is_none()
    });
    for files in by_run.values() {
        let count = files[0].1.run_count;
        if files.iter().any(|(_, l, _)| l.run_count != count) {
            return Err(breach(
                4,
                files[0].0,
                "files of one run disagree on its count",
            ));
        }
        if files.len() > count as usize {
            return Err(breach(4, files[0].0, "more files of a run than its count"));
        }
        if files.len() < count as usize {
            if !unlaid {
                return Err(breach(4, files[0].0, "fewer files of a run than its count"));
            }
            s.unknown.extend(files.iter().map(|f| f.0));
        }
        let inputs: BTreeSet<FileRef> = files.iter().flat_map(|f| f.2.iter().copied()).collect();
        if inputs.iter().any(|i| i.kind == kind::LOG_SEGMENT) {
            continue;
        }
        let mut sum_in = Totals::default();
        let mut known = true;
        for i in &inputs {
            match s.layout(*i) {
                Some(l) => sum_in.add(l),
                None => known = false,
            }
        }
        let mut sum_out = Totals::default();
        for (_, l, _) in files {
            sum_out.add(l);
        }
        if known && files.len() == count as usize && sum_in != sum_out {
            return Err(breach(
                3,
                files[0].0,
                "a merge whose counts differ from its inputs'",
            ));
        }
    }
    Ok(())
}

#[derive(Default, PartialEq, Eq)]
struct Totals {
    facts: u128,
    per_generation: BTreeMap<u32, u128>,
}

impl Totals {
    fn add(&mut self, l: &CompactedLayout) {
        self.facts += u128::from(l.fact_count);
        for (g, n) in &l.positions {
            *self.per_generation.entry(*g).or_default() += u128::from(*n);
        }
    }
}

/// C(g) for every generation with a compacted prefix: the last position of that prefix. A catalogue that breaks
/// rule 2 has none past the first skipped segment, so a skipped segment's positions are never served from chunks.
pub fn compacted_through(c: &Catalogue) -> BTreeMap<u32, u64> {
    let named = named_segments(c);
    segments(c)
        .into_iter()
        .filter_map(|(g, segs)| prefix(&segs, &named).0.map(|e| (g, e)))
        .collect()
}

/// The shape of a new publish record (§3.4 rule 4): its compacted outputs are all of a run, which retires nothing,
/// or all of a merge, which retires exactly the compacted files its outputs name.
pub fn publish_shape(p: &Publish) -> Result<(), Breach> {
    let outputs: Vec<_> = p
        .published
        .iter()
        .filter(|d| d.kind == kind::COMPACTED)
        .collect();
    let Some(first) = outputs.first() else {
        return Ok(());
    };
    let is_run =
        |d: &&&crate::desc::FileDescription| d.inputs.iter().all(|i| i.kind == kind::LOG_SEGMENT);
    let bad = |file: FileRef, what| {
        Err(Breach {
            rule: 4,
            file,
            what,
        })
    };
    if outputs.iter().any(|d| is_run(&d) != is_run(&first)) {
        return bad(
            first.file_ref(),
            "a record publishing both a run and a merge",
        );
    }
    let retired: BTreeSet<FileRef> = p.retired.iter().map(|d| d.file_ref()).collect();
    let named: BTreeSet<FileRef> = if is_run(&first) {
        BTreeSet::new()
    } else {
        outputs
            .iter()
            .flat_map(|d| d.inputs.iter().copied())
            .collect()
    };
    if retired != named {
        return bad(
            first.file_ref(),
            "a record that does not retire exactly the compacted files its outputs name",
        );
    }
    Ok(())
}
