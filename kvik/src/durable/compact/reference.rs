//! The whole-in-memory run of roadmap 6.2a2, kept as the oracle the stepped run is compared with and as the writer
//! the verification tests tamper with.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use kvik_chunk::file::{
    Batch, Class, Description, Fact, FileWriter, Identity, Input, Level, Position, Series,
};
use kvik_log::BodyError;
use kvik_log::FormatError;
use kvik_log::batch::SegmentIdentity;
use kvik_log::content::{BatchContent, Item};
use kvik_log::entries::{Definition, Entry};
use kvik_log::fs::OpenMode;
use kvik_log::scan::{HeaderState, ScanLimit, SealState, scan};

use super::plan::Candidate;
use super::run::{DATA_DIR, RunInput, info, rank, read_whole, tmp_path};
use super::verify::{Anchors, Check, Expect, Written};
use super::{
    Cause, KIND_LOG, Output, RunError, Stall, chunk_fact, entry_digest, fact_digest, period,
};

/// One position of the inputs.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Row {
    pub(super) received_at: i64,
    pub(super) commit_time: i64,
    pub(super) item: Taken,
}

/// What a position holds.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Taken {
    Fact { fact: Fact, set_aside: bool },
    Entry(Entry),
}

/// Everything a run took from its inputs.
pub(super) struct Inputs {
    pub(super) rows: BTreeMap<Position, Row>,
    pub(super) batches: Vec<Batch>,
    pub(super) anchors: Anchors,
    pub(super) last_commit_time: i64,
    pub(super) names: Vec<Input>,
    pub(super) definitions: BTreeMap<u32, Vec<(Position, Definition)>>,
    /// Each input's first and last position, generation and `file_id`, from its trailer.
    pub(super) ranges: Vec<(Position, Position, u32, u64)>,
}

impl Inputs {
    /// A stall at the input that holds `p`.
    fn stall_at(&self, p: Position, cause: Cause) -> RunError {
        let (generation, file_id) = self
            .ranges
            .iter()
            .find(|r| r.0 <= p && p <= r.1)
            .map_or((p.0, 0), |r| (r.2, r.3));
        RunError::Stalled(Stall {
            generation,
            file_id,
            cause,
        })
    }
}

/// The verified outputs, or none: on a failed check every output is removed and the inputs stay as they are (§3.2).
pub(super) fn verified_or_removed(
    input: &RunInput<'_>,
    taken: &Inputs,
    written: &[Written],
) -> Result<Vec<Output>, RunError> {
    let mut expect = expect(taken)?;
    let ranges: Vec<(Position, Position)> = taken.ranges.iter().map(|r| (r.0, r.1)).collect();
    let mut check = Check::new(written.to_vec(), &ranges);
    let infos = |s: u32, g: Position| {
        let (name, reach) = info(input, &taken.definitions, s, g);
        kvik_chunk::file::SeriesInfo { name, reach }
    };
    loop {
        match check.step(input, &mut expect, &infos) {
            Ok(None) => {}
            Ok(Some(out)) => return Ok(out),
            Err(e) => {
                for w in written {
                    let _ = input.fs.remove_file(&w.path);
                }
                return Err(e.into());
            }
        }
    }
}

/// What the taken rows promise, as the census computes it.
fn expect(taken: &Inputs) -> Result<Expect, RunError> {
    let mut e = Expect {
        anchors: taken.anchors.clone(),
        names: taken.names.clone(),
        ..Expect::default()
    };
    for (&p, row) in &taken.rows {
        match &row.item {
            Taken::Fact { fact, .. } => {
                let s = e.sums.entry(fact.series_id).or_default();
                *s = s.wrapping_add(fact_digest(fact, row.received_at, row.commit_time));
            }
            Taken::Entry(x) => {
                let d = entry_digest(p, x, row.received_at, row.commit_time)
                    .map_err(|_| super::VerifyError::Checksum(None))?;
                e.entries = e.entries.wrapping_add(d);
            }
        }
    }
    Ok(e)
}

pub(super) fn take(input: &RunInput<'_>, segments: &[Candidate]) -> Result<Inputs, RunError> {
    let mut rows: BTreeMap<Position, Row> = BTreeMap::new();
    let mut batches: BTreeMap<Position, Batch> = BTreeMap::new();
    let mut anchors = Anchors::default();
    let mut definitions: BTreeMap<u32, Vec<(Position, Definition)>> = BTreeMap::new();
    let mut ranges = Vec::new();
    for c in segments {
        let stall = |cause| {
            RunError::Stalled(Stall {
                generation: c.generation,
                file_id: c.file_id,
                cause,
            })
        };
        let bytes = read_whole(&*input.fs.open(&c.path, OpenMode::Read)?)?;
        let id = SegmentIdentity {
            store_id: input.store_id,
            file_id: c.file_id,
            generation: c.generation,
        };
        let report = scan(&bytes, &id, ScanLimit::End).map_err(|e| match e {
            FormatError::Unsupported { .. } => stall(Cause::Unknown),
            _ => stall(Cause::Unreadable("refused by the scan")),
        })?;
        let seal = report
            .seal
            .as_ref()
            .filter(|s| s.state == SealState::Sealed && s.file_crc_matches(&bytes));
        let (Some(seal), HeaderState::Valid(_), true) = (
            seal,
            report.header,
            report.regions.is_empty() && report.tail.is_none(),
        ) else {
            return Err(stall(Cause::Unreadable("not sealed whole")));
        };
        let Some(Some(index)) = &seal.index else {
            return Err(stall(Cause::Unreadable("no segment index")));
        };
        let t = &seal.trailer;
        ranges.push((
            (c.generation, t.first_sequence),
            (c.generation, t.last_sequence),
            c.generation,
            c.file_id,
        ));
        let mut positions = 0u64;
        let mut facts: BTreeMap<u32, u64> = BTreeMap::new();
        for batch in &report.batches {
            let content = BatchContent::decode_canonical(batch).map_err(|e| match e {
                BodyError::NonCanonical => stall(Cause::Unknown),
                _ => stall(Cause::Unreadable("a batch body")),
            })?;
            let commit = batch.commit;
            if content.first_sequence != commit.first_sequence
                || content.last_sequence() != commit.last_sequence
            {
                return Err(stall(Cause::Unreadable(
                    "a batch disagrees with its commit",
                )));
            }
            let set_aside = kvik_log::scan::set_aside(batch, input.set_aside);
            let first = (commit.generation, commit.first_sequence);
            let b = Batch {
                generation: commit.generation,
                first_sequence: commit.first_sequence,
                last_sequence: commit.last_sequence,
                received_at: content.received_at,
                commit_time: commit.commit_time,
            };
            match batches.get(&first) {
                Some(old) if *old != b => return Err(RunError::Conflict(first)),
                _ => {
                    batches.insert(first, b);
                }
            }
            let mut seq = content.first_sequence;
            for item in &content.items {
                let p = (commit.generation, seq);
                let taken = match item {
                    Item::Fact(r) => {
                        *facts.entry(r.series_id).or_default() += 1;
                        Taken::Fact {
                            fact: chunk_fact(r, p),
                            set_aside,
                        }
                    }
                    Item::Entry(e) => match &**e {
                        Entry::Ignored { .. } => return Err(stall(Cause::Unknown)),
                        _ if set_aside => return Err(stall(Cause::SetAsideEntry)),
                        Entry::Definition(d) => {
                            definitions
                                .entry(d.id)
                                .or_default()
                                .push((p, (**d).clone()));
                            Taken::Entry((**e).clone())
                        }
                        other => Taken::Entry(other.clone()),
                    },
                };
                positions += 1;
                seq += item.sequences();
                let row = Row {
                    received_at: content.received_at,
                    commit_time: commit.commit_time,
                    item: taken,
                };
                match rows.get(&p) {
                    Some(old) if *old != row => return Err(RunError::Conflict(p)),
                    Some(old) => {
                        anchors.positions = anchors.positions.wrapping_sub(1);
                        if let Taken::Fact { fact, .. } = &old.item {
                            let n = anchors.facts.entry(fact.series_id).or_default();
                            *n = n.wrapping_sub(1);
                        }
                    }
                    None => {
                        rows.insert(p, row);
                    }
                }
            }
        }
        let sealed: BTreeMap<u32, u64> = index
            .records
            .iter()
            .map(|r| (r.series_id, u64::from(r.fact_count)))
            .filter(|&(_, n)| n > 0)
            .collect();
        if positions != seal.trailer.entry_count || facts != sealed {
            return Err(stall(Cause::Unreadable("counts disagree with its seal")));
        }
        anchors.positions = anchors.positions.wrapping_add(seal.trailer.entry_count);
        for (s, n) in sealed {
            let a = anchors.facts.entry(s).or_default();
            *a = a.wrapping_add(n);
        }
    }
    anchors.facts.retain(|_, n| *n > 0);
    let mut last: BTreeMap<u32, i64> = BTreeMap::new();
    let mut decreasing = None;
    for b in batches.values() {
        if last.get(&b.generation).is_some_and(|&t| b.commit_time < t) {
            decreasing = Some((b.generation, b.first_sequence));
            break;
        }
        last.insert(b.generation, b.commit_time);
    }
    let batches: Vec<Batch> = batches.into_values().collect();
    let last_commit_time = batches
        .iter()
        .map(|b| b.commit_time)
        .max()
        .unwrap_or(i64::MIN);
    let names: BTreeSet<Input> = segments
        .iter()
        .map(|c| Input {
            kind: KIND_LOG,
            file_id: c.file_id,
        })
        .collect();
    let inputs = Inputs {
        rows,
        batches,
        anchors,
        last_commit_time,
        names: names.into_iter().collect(),
        definitions,
        ranges,
    };
    match decreasing {
        Some(p) => Err(inputs.stall_at(p, Cause::CommitTimeDecreases)),
        None => Ok(inputs),
    }
}

/// The rows of one period, by class.
#[derive(Default)]
pub(super) struct Part {
    pub(super) current: BTreeMap<u32, Vec<Fact>>,
    pub(super) revision: BTreeMap<u32, Vec<Fact>>,
    pub(super) set_aside: BTreeMap<u32, Vec<Fact>>,
    pub(super) entries: Vec<(Position, Entry)>,
}

pub(super) fn place(
    input: &RunInput<'_>,
    taken: &Inputs,
) -> Result<BTreeMap<(i64, i64), Part>, RunError> {
    let mut parts: BTreeMap<(i64, i64), Part> = BTreeMap::new();
    let mut keyed: Vec<(Key, (u64, Position), Fact)> = Vec::new();
    for (&p, row) in &taken.rows {
        let when = match &row.item {
            Taken::Fact { fact, .. } => fact.t,
            Taken::Entry(_) => row.commit_time,
        };
        let Some(key) = period(when, taken.last_commit_time, input.weeks_merged) else {
            return Err(taken.stall_at(p, Cause::PeriodOutOfRange));
        };
        match &row.item {
            Taken::Entry(e) => parts.entry(key).or_default().entries.push((p, e.clone())),
            Taken::Fact {
                fact,
                set_aside: true,
            } => parts
                .entry(key)
                .or_default()
                .set_aside
                .entry(fact.series_id)
                .or_default()
                .push(fact.clone()),
            Taken::Fact { fact, .. } => {
                keyed.push(((fact.series_id, fact.t), rank(fact), fact.clone()))
            }
        }
    }
    keyed.sort_by_key(|k| (k.0, k.1));
    for i in 0..keyed.len() {
        let top = keyed.get(i + 1).is_none_or(|next| next.0 != keyed[i].0);
        let f = &keyed[i].2;
        let key = period(f.t, taken.last_commit_time, input.weeks_merged).expect("placed above");
        let part = parts.entry(key).or_default();
        let class = if top {
            &mut part.current
        } else {
            &mut part.revision
        };
        class.entry(f.series_id).or_default().push(f.clone());
    }
    Ok(parts)
}

/// A series of a file: its id, its facts and its set-aside facts.
type SeriesRows = (u32, Vec<Fact>, Vec<Fact>);
/// The non-fact entries of a file.
type Entries = Vec<(Position, Entry)>;
/// A key: series and `t`.
type Key = (u32, i64);

/// Writes one file per period and class under its `.tmp` name; on any error every file it created is removed (§3.5).
pub(super) fn write(
    input: &RunInput<'_>,
    taken: &Inputs,
    parts: BTreeMap<(i64, i64), Part>,
) -> Result<Vec<Written>, RunError> {
    let mut created = Vec::new();
    write_files(input, taken, parts, &mut created).inspect_err(|_| {
        for p in &created {
            let _ = input.fs.remove_file(p);
        }
    })
}

fn write_files(
    input: &RunInput<'_>,
    taken: &Inputs,
    parts: BTreeMap<(i64, i64), Part>,
    created: &mut Vec<std::path::PathBuf>,
) -> Result<Vec<Written>, RunError> {
    let mut files: Vec<(i64, i64, Class, Vec<SeriesRows>, Entries)> = Vec::new();
    for ((start, len), part) in parts {
        if !part.current.is_empty() || !part.entries.is_empty() {
            let series = part
                .current
                .into_iter()
                .map(|(s, f)| (s, f, Vec::new()))
                .collect();
            files.push((start, len, Class::Current, series, part.entries));
        }
        if !part.revision.is_empty() || !part.set_aside.is_empty() {
            let mut series: BTreeMap<u32, (Vec<Fact>, Vec<Fact>)> = BTreeMap::new();
            for (s, f) in part.revision {
                series.entry(s).or_default().0 = f;
            }
            for (s, f) in part.set_aside {
                series.entry(s).or_default().1 = f;
            }
            let series = series.into_iter().map(|(s, (f, a))| (s, f, a)).collect();
            files.push((start, len, Class::Revision, series, Vec::new()));
        }
    }
    let run_count = u32::try_from(files.len()).map_err(|_| io::Error::other("too many outputs"))?;
    let data = input.dir.join(DATA_DIR);
    input.fs.create_dir_all(&data)?;
    let mut out = Vec::new();
    for (i, (start, len, class, series, entries)) in files.into_iter().enumerate() {
        let id = Identity {
            store_id: input.store_id,
            generation: input.generation,
            file_id: input.first_file_id + i as u64,
        };
        let mut held: BTreeMap<u32, u64> = BTreeMap::new();
        let mut greatest = (0, 0);
        for p in series
            .iter()
            .flat_map(|(_, f, a)| f.iter().chain(a))
            .map(|f| f.position)
            .chain(entries.iter().map(|e| e.0))
        {
            *held.entry(p.0).or_default() += 1;
            greatest = greatest.max(p);
        }
        let description = Description {
            class,
            level: Level::L0,
            period_start: start,
            period_len: len,
            run_id: input.first_file_id,
            run_count,
            inputs: taken.names.clone(),
            name_range: None,
            positions: held.into_iter().collect(),
            has_entries: !entries.is_empty(),
            has_set_aside: series.iter().any(|(_, _, a)| !a.is_empty()),
            complete: true,
        };
        let mut named: Vec<Series> = series
            .into_iter()
            .map(|(series_id, facts, set_aside)| {
                let (name, reach) = info(input, &taken.definitions, series_id, greatest);
                Series {
                    series_id,
                    name,
                    reach,
                    facts,
                    set_aside,
                }
            })
            .collect();
        named.sort_by(|a, b| {
            (
                a.name.is_none(),
                a.name.as_deref().map(str::as_bytes),
                a.series_id,
            )
                .cmp(&(
                    b.name.is_none(),
                    b.name.as_deref().map(str::as_bytes),
                    b.series_id,
                ))
        });
        let mut w = FileWriter::new(
            Vec::new(),
            id,
            input.created_at,
            &description,
            taken.batches.clone(),
        )?;
        for s in named {
            w.series(s)?;
        }
        w.entries(&entries)?;
        let (bytes, written) = w.finish()?;
        let path = tmp_path(input.dir, &id);
        match input.fs.remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let f = input.fs.open(&path, OpenMode::CreateNew)?;
        created.push(path.clone());
        f.write_at(&bytes, 0)?;
        f.sync_all()?;
        out.push(Written {
            id,
            layout: written.layout,
            len: written.len,
            path,
            period: (start, len),
            class,
            greatest,
        });
    }
    input.fs.sync_dir(&data)?;
    Ok(out)
}
