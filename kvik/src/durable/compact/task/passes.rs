//! The passes: each collects its group's rows from the inputs, counts or writes them, and drains and syncs the
//! outputs a piece a step (compaction design §3.1, §4; Q-1535, Q-1940).

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::ops::Range;
use std::path::PathBuf;

use kvik_chunk::file::{
    Batch, CHUNK_ROWS, Class, Description, Fact, FileWriter, Identity, Level, Position,
};
use kvik_log::content::Item;
use kvik_log::entries::{Definition, Entry};
use kvik_log::fs::{FsFile, OpenMode};

use super::super::run::{DATA_DIR, RunInput, info, rank, tmp_path};
use super::super::segment::{Decoded, Event, PIECE, Segment};
use super::super::verify::Written;
use super::super::{Output, RunError, chunk_fact};

use super::census::{Group, Plan};
use super::{RunTask, SYNC_EVERY, Stage};

pub(super) const CURRENT: u8 = 0;
pub(super) const REVISION: u8 = 1;

/// Bytes a row held in a pass costs: the fact, its text at full length and its sort key (§4: real size).
pub(super) fn row_cost(text: usize) -> u64 {
    (std::mem::size_of::<PRow>() + text) as u64
}

/// One row of a pass.
pub(super) struct PRow {
    pub(super) period: usize,
    pub(super) class: u8,
    pub(super) order: usize,
    pub(super) aside: bool,
    pub(super) fact: Fact,
}

/// What a file will hold, from the count passes.
#[derive(Default)]
pub(super) struct Meta {
    pub(super) positions: BTreeMap<u32, u64>,
    pub(super) greatest: Position,
    pub(super) set_aside: bool,
    pub(super) entries: bool,
    pub(super) used: Vec<u64>,
    /// Its series (a series' set-aside rows counted again) and rows, so its writer's index is allocated once.
    pub(super) series: u64,
    pub(super) rows: u64,
}

impl Meta {
    pub(super) fn hold(&mut self, p: Position, batch: usize) {
        *self.positions.entry(p.0).or_default() += 1;
        self.greatest = self.greatest.max(p);
        if self.used.len() <= batch / 64 {
            self.used.resize(batch / 64 + 1, 0);
        }
        self.used[batch / 64] |= 1 << (batch % 64);
    }
}

/// An output being written.
pub(super) struct Open {
    pub(super) file: usize,
    pub(super) id: Identity,
    pub(super) w: Option<FileWriter<Vec<u8>>>,
    pub(super) tail: Vec<u8>,
    /// The file, once its stale copy is removed (`removed`) and it is created, each a step of its own.
    pub(super) out: Option<Box<dyn FsFile>>,
    pub(super) removed: bool,
    pub(super) path: PathBuf,
    pub(super) at: u64,
    pub(super) unsynced: u64,
    pub(super) series: Option<u32>,
    /// Its writer is past its last series; the index goes out a block a step.
    pub(super) finishing: bool,
    pub(super) written: Option<Written>,
}

impl Open {
    pub(super) fn file(&self) -> &dyn FsFile {
        self.out.as_deref().expect("opened")
    }
}

/// The rows of the open pass.
#[derive(Default)]
pub(super) struct Rows {
    pub(super) seg: usize,
    pub(super) segment: Option<Segment>,
    pub(super) rows: Vec<PRow>,
    pub(super) collected: bool,
    pub(super) opened: bool,
    /// What `rows` costs: its capacity and the texts' bytes (Q-1841).
    pub(super) bytes: u64,
    pub(super) text: u64,
    pub(super) next: usize,
}

impl RunTask<'_> {
    pub(super) fn pass_step(&mut self) -> Result<Option<Vec<Output>>, RunError> {
        let plan = self.plan.as_mut().expect("planned");
        let Stage::Pass { k, rows } = &mut self.stage else {
            unreachable!("in a pass")
        };
        let Some(pass) = plan.passes.get(*k).copied() else {
            self.begin_verify()?;
            return Ok(None);
        };
        let (window, group) = (
            plan.windows[pass.window].clone(),
            plan.groups[pass.group].clone(),
        );
        let first_of_window = pass.group == 0;
        let last_of_window = pass.group + 1 == plan.groups.len();
        if !rows.collected {
            if rows.seg == 0 && rows.segment.is_none() && rows.rows.capacity() == 0 {
                let want = match plan.taken.get(&(pass.window, pass.group)) {
                    Some(&n) => n as u64,
                    None => {
                        let in_window: u64 = plan.period_rows[window.clone()].iter().sum();
                        let most = self.cap / std::mem::size_of::<PRow>() as u64;
                        group.rows.min(in_window).min(most)
                    }
                };
                rows.rows.reserve_exact(usize::try_from(want).unwrap_or(0));
                rows.bytes = (rows.rows.capacity() * std::mem::size_of::<PRow>()) as u64;
            }
            let Some(c) = self.segments.get(rows.seg) else {
                rows.collected = true;
                if pass.count {
                    plan.taken
                        .insert((pass.window, pass.group), rows.rows.len());
                }
                place(plan, &mut rows.rows, &mut self.entries, pass.count);
                if pass.count && pass.window + 1 == plan.windows.len() && last_of_window {
                    plan.files = plan.metas.keys().copied().collect();
                }
                if pass.write && plan.renamed {
                    by_file_names(&self.input, &self.census.defs, plan, &mut rows.rows);
                }
                return Ok(None);
            };
            let Some(s) = &mut rows.segment else {
                rows.segment = Some(Segment::open(self.input.fs, c)?);
                return Ok(None);
            };
            match s.step(self.input.store_id, self.input.set_aside)? {
                Event::Pending => {}
                Event::End => {
                    rows.segment = None;
                    rows.seg += 1;
                }
                Event::Batch(d) => {
                    rows.text += collect(
                        plan,
                        self.input.weeks_merged,
                        &window,
                        &group,
                        first_of_window,
                        d,
                        &mut rows.rows,
                        &mut self.entries,
                    );
                    rows.bytes =
                        (rows.rows.capacity() * std::mem::size_of::<PRow>()) as u64 + rows.text;
                }
            }
            return Ok(None);
        }
        if !pass.write {
            *k += 1;
            *rows = Rows::default();
            self.entries.clear();
            return Ok(None);
        }
        if !rows.opened && first_of_window {
            rows.opened = true;
            for i in (0..plan.files.len()).filter(|&i| window.contains(&plan.files[i].0)) {
                self.open.push(new_output(&self.input, plan, i)?);
            }
            return Ok(None);
        }
        if !self.data_dir && !self.open.is_empty() {
            self.input
                .fs
                .create_dir_all(&self.input.dir.join(DATA_DIR))?;
            self.data_dir = true;
            return Ok(None);
        }
        if let Some(o) = self.open.iter_mut().find(|o| o.out.is_none()) {
            if o.removed {
                o.out = Some(self.input.fs.open(&o.path, OpenMode::CreateNew)?);
                self.created.push(o.path.clone());
            } else {
                match self.input.fs.remove_file(&o.path) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                    _ => o.removed = true,
                }
            }
            return Ok(None);
        }
        if let Some(o) = self.open.iter_mut().find(|o| pending(o) >= PIECE) {
            drain(o)?;
            return Ok(None);
        }
        if let Some(o) = self.open.iter_mut().find(|o| o.unsynced >= SYNC_EVERY) {
            o.file().sync_all()?;
            o.unsynced = 0;
            return Ok(None);
        }
        if rows.next < rows.rows.len() {
            write_rows(&self.input, &self.census.defs, plan, &mut self.open, rows)?;
            return Ok(None);
        }
        if !group.continues() {
            for o in &mut self.open {
                if o.series.take().is_some() {
                    o.w.as_mut().expect("open").end_series()?;
                }
            }
        }
        if !last_of_window {
            *k += 1;
            *rows = Rows::default();
            return Ok(None);
        }
        if let Some(o) = self.open.iter_mut().find(|o| o.w.is_some()) {
            let (pi, class) = plan.files[o.file];
            let w = o.w.as_mut().expect("checked");
            if !o.finishing {
                o.finishing = true;
                if class == CURRENT
                    && let Some(e) = self.entries.remove(&pi)
                {
                    w.entries(&e)?;
                }
            }
            if !w.finish_step()? {
                return Ok(None);
            }
            let (sink, done) = o.w.take().expect("checked").finish()?;
            o.tail = sink;
            let meta = &plan.metas[&plan.files[o.file]];
            o.written = Some(Written {
                id: o.id,
                layout: done.layout,
                len: done.len,
                path: o.path.clone(),
                period: plan.periods[pi],
                class: if class == CURRENT {
                    Class::Current
                } else {
                    Class::Revision
                },
                greatest: meta.greatest,
            });
            return Ok(None);
        }
        if let Some(o) = self.open.iter_mut().find(|o| !o.tail.is_empty()) {
            drain(o)?;
            return Ok(None);
        }
        if let Some(i) = self.open.iter().position(|o| o.written.is_some()) {
            let mut o = self.open.swap_remove(i);
            o.file().sync_all()?;
            self.written.push(o.written.take().expect("finished"));
            return Ok(None);
        }
        self.written.sort_by_key(|w| w.id.file_id);
        *k += 1;
        *rows = Rows::default();
        self.entries.clear();
        Ok(None)
    }
}

/// Takes the rows of one batch that fall in the pass: its group's facts in its window, and, in a window's first pass,
/// the window's entries.
#[allow(clippy::too_many_arguments)]
pub(super) fn collect(
    plan: &Plan,
    weeks: &BTreeSet<i64>,
    window: &Range<usize>,
    group: &Group,
    first: bool,
    d: Decoded,
    rows: &mut Vec<PRow>,
    entries: &mut BTreeMap<usize, Vec<(Position, Entry)>>,
) -> u64 {
    let before = rows.len();
    let mut seq = d.content.first_sequence;
    for item in d.content.items {
        let p = (d.commit.generation, seq);
        seq += item.sequences();
        match item {
            Item::Fact(r) => {
                let Some(&order) = plan.order.get(&r.series_id) else {
                    continue;
                };
                let Some(pi) = plan.period_of(r.t, weeks) else {
                    continue;
                };
                if group.takes(order, r.t, d.set_aside) && window.contains(&pi) {
                    rows.push(PRow {
                        period: pi,
                        class: CURRENT,
                        order,
                        aside: d.set_aside,
                        fact: chunk_fact(&r, p),
                    });
                }
            }
            Item::Entry(e) if first => {
                if let Some(pi) = plan.period_of(d.commit.commit_time, weeks)
                    && window.contains(&pi)
                {
                    entries.entry(pi).or_default().push((p, *e));
                }
            }
            Item::Entry(_) => {}
        }
    }
    rows[before..]
        .iter()
        .map(|r| r.fact.text.capacity() as u64)
        .sum()
}

/// Ranks the pass's rows into classes (§3.1 Class), keeps a position met twice once, counts them into the files'
/// metas when `count`, and sorts them in write order.
pub(super) fn place(
    plan: &mut Plan,
    rows: &mut Vec<PRow>,
    entries: &mut BTreeMap<usize, Vec<(Position, Entry)>>,
    count: bool,
) {
    // Unstable sorts hold no scratch; their keys end in the position, so equal keys are the same row met twice.
    rows.sort_unstable_by(|a, b| {
        (a.aside, a.fact.series_id, a.fact.t, rank(&a.fact)).cmp(&(
            b.aside,
            b.fact.series_id,
            b.fact.t,
            rank(&b.fact),
        ))
    });
    rows.dedup_by(|b, a| {
        a.fact.position == b.fact.position && a.fact.series_id == b.fact.series_id
    });
    for i in 0..rows.len() {
        let top = !rows[i].aside
            && rows.get(i + 1).is_none_or(|n| {
                n.aside || (n.fact.series_id, n.fact.t) != (rows[i].fact.series_id, rows[i].fact.t)
            });
        rows[i].class = if top { CURRENT } else { REVISION };
    }
    for e in entries.values_mut() {
        e.sort_unstable_by_key(|x| x.0);
        e.dedup_by_key(|x| x.0);
    }
    if count {
        for r in rows.iter() {
            let b = plan.batch_of(r.fact.position);
            plan.metas
                .entry((r.period, r.class))
                .or_default()
                .hold(r.fact.position, b);
            if r.aside {
                plan.metas
                    .get_mut(&(r.period, r.class))
                    .expect("held")
                    .set_aside = true;
            }
        }
        for (&pi, e) in entries.iter() {
            for (p, _) in e {
                let b = plan.batch_of(*p);
                let m = plan.metas.entry((pi, CURRENT)).or_default();
                m.hold(*p, b);
                m.entries = true;
            }
        }
    }
    rows.sort_unstable_by(|a, b| {
        (
            a.period,
            a.class,
            a.order,
            !a.aside,
            a.fact.t,
            a.fact.position,
        )
            .cmp(&(
                b.period,
                b.class,
                b.order,
                !b.aside,
                b.fact.t,
                b.fact.position,
            ))
    });
    if count {
        let mut last = None;
        for r in rows.iter() {
            let m = plan.metas.get_mut(&(r.period, r.class)).expect("held");
            m.rows += 1;
            let run = Some((r.period, r.class, r.order, r.aside));
            if last != run {
                m.series += 1;
                last = run;
            }
        }
    }
}

/// Orders a pass's rows by each file's own names, which differ between files when a series is renamed in the inputs.
pub(super) fn by_file_names(
    input: &RunInput<'_>,
    defs: &BTreeMap<u32, Vec<(Position, Definition)>>,
    plan: &Plan,
    rows: &mut [PRow],
) {
    let mut named: BTreeMap<(usize, u8, u32), (bool, Option<String>)> = BTreeMap::new();
    for r in rows.iter() {
        let key = (r.period, r.class, r.fact.series_id);
        named.entry(key).or_insert_with(|| {
            let name = info(input, defs, key.2, plan.metas[&(key.0, key.1)].greatest).0;
            (name.is_none(), name)
        });
    }
    let mut sorted: Vec<_> = named.into_iter().collect();
    sorted.sort_by(|a, b| {
        let k = |x: &((usize, u8, u32), (bool, Option<String>))| {
            (
                x.0.0,
                x.0.1,
                x.1.0,
                x.1.1.clone().map(String::into_bytes),
                x.0.2,
            )
        };
        k(a).cmp(&k(b))
    });
    let order: BTreeMap<(usize, u8, u32), usize> = sorted
        .into_iter()
        .enumerate()
        .map(|(i, (k, _))| (k, i))
        .collect();
    for r in rows.iter_mut() {
        r.order = order[&(r.period, r.class, r.fact.series_id)];
    }
    rows.sort_unstable_by(|a, b| {
        (
            a.period,
            a.class,
            a.order,
            !a.aside,
            a.fact.t,
            a.fact.position,
        )
            .cmp(&(
                b.period,
                b.class,
                b.order,
                !b.aside,
                b.fact.t,
                b.fact.position,
            ))
    });
}

/// Creates output `i` and its writer.
pub(super) fn new_output(input: &RunInput<'_>, plan: &Plan, i: usize) -> Result<Open, RunError> {
    let key = plan.files[i];
    let meta = &plan.metas[&key];
    let (start, len) = plan.periods[key.0];
    let id = Identity {
        store_id: input.store_id,
        generation: input.generation,
        file_id: input.first_file_id + i as u64,
    };
    let description = Description {
        class: if key.1 == CURRENT {
            Class::Current
        } else {
            Class::Revision
        },
        level: Level::L0,
        period_start: start,
        period_len: len,
        run_id: input.first_file_id,
        run_count: u32::try_from(plan.files.len())
            .map_err(|_| io::Error::other("too many outputs"))?,
        inputs: plan.names.clone(),
        name_range: None,
        positions: meta.positions.iter().map(|(&g, &n)| (g, n)).collect(),
        has_entries: meta.entries,
        has_set_aside: meta.set_aside,
        complete: true,
    };
    let batches: Vec<Batch> = plan
        .batches
        .iter()
        .enumerate()
        .filter(|(b, _)| {
            meta.used
                .get(b / 64)
                .is_some_and(|w| w & (1 << (b % 64)) != 0)
        })
        .map(|(_, b)| *b)
        .collect();
    let mut w = FileWriter::new(Vec::new(), id, input.created_at, &description, batches)?;
    w.reserve_index(usize::try_from(meta.series + meta.rows / CHUNK_ROWS as u64).unwrap_or(0));
    Ok(Open {
        file: i,
        id,
        w: Some(w),
        tail: Vec::new(),
        out: None,
        removed: false,
        path: tmp_path(input.dir, &id),
        at: 0,
        unsynced: 0,
        series: None,
        finishing: false,
        written: None,
    })
}

pub(super) fn pending(o: &Open) -> usize {
    o.w.as_ref().map_or(o.tail.len(), |w| w.sink().len())
}

/// Writes at most one piece of an output's sink to its file.
pub(super) fn drain(o: &mut Open) -> io::Result<()> {
    let b = match &mut o.w {
        Some(w) => w.sink_mut(),
        None => &mut o.tail,
    };
    let n = b.len().min(PIECE);
    o.out.as_ref().expect("opened").write_at(&b[..n], o.at)?;
    b.drain(..n);
    o.at += n as u64;
    o.unsynced += n as u64;
    Ok(())
}

/// Pushes rows of the pass to their writers until one data block is encoded or a series ends.
pub(super) fn write_rows(
    input: &RunInput<'_>,
    defs: &BTreeMap<u32, Vec<(Position, Definition)>>,
    plan: &Plan,
    open: &mut [Open],
    rows: &mut Rows,
) -> Result<(), RunError> {
    let r = &rows.rows[rows.next];
    let file = plan
        .files
        .binary_search(&(r.period, r.class))
        .expect("counted");
    let o = open.iter_mut().find(|o| o.file == file).expect("opened");
    let w = o.w.as_mut().expect("open");
    let series = r.fact.series_id;
    if o.series != Some(series) {
        if o.series.take().is_some() {
            w.end_series()?;
        }
        let greatest = plan.metas[&plan.files[file]].greatest;
        let (name, reach) = info(input, defs, series, greatest);
        let mut aside = Vec::new();
        while let Some(r) = rows.rows.get(rows.next).filter(|r| {
            r.aside && r.fact.series_id == series && plan.files[file] == (r.period, r.class)
        }) {
            aside.push(r.fact.clone());
            rows.next += 1;
        }
        w.begin_series(series, name.as_deref(), reach, aside)?;
        o.series = Some(series);
        return Ok(());
    }
    let blocks = w.data_blocks();
    while let Some(r) = rows.rows.get(rows.next) {
        if r.fact.series_id != series || plan.files[file] != (r.period, r.class) {
            break;
        }
        w.push(r.fact.clone())?;
        rows.next += 1;
        if w.data_blocks() != blocks || w.sink_mut().len() >= PIECE {
            break;
        }
    }
    Ok(())
}
