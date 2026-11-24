//! A lookup's reads of sealed segments through their series batches (`format.md` A.8a, Q-1150): planned under the
//! reader, read apart from it, so that lookups of several callers read their batches at once.

use super::*;

/// Batches of a listed read closer than this are read together, as a read call costs about as much as copying this
/// many bytes more.
const LISTED_GAP: u64 = 4 << 10;

/// One sealed segment a lookup reads through its series batches: what the read needs, taken from the reader's caches.
pub(super) struct Listed {
    name: SegmentName,
    file: Box<dyn crate::fs::FsFile>,
    id: SegmentIdentity,
    /// The series batches block's offset and length, header included.
    lists: (u64, u64),
    batch_bytes: u64,
    /// The series the segment index lists, ascending: the block must list exactly these.
    index_ids: Vec<u32>,
    /// The wanted ranges the segment may hold, by their index, and their series, ascending and once each.
    candidates: Vec<usize>,
    ids: Vec<u32>,
    by_series: BTreeMap<u32, Vec<(i64, i64)>>,
    ranges: Vec<(i64, i64)>,
}

/// The reads a lookup left for [`Planned::run`], with the lookup's budget and position bounds.
pub struct Planned {
    pub(super) jobs: Vec<Listed>,
    pub(super) budget: Budget,
    pub(super) generation: u32,
    pub(super) last_sequence: u64,
    pub(super) above: Option<Position>,
    pub(super) upto: Option<Position>,
}

/// What [`Planned::run`] did: the reads it left to the reader (by their place in the plan), and the segments it took
/// a fact from.
#[derive(Debug, Default)]
pub struct Ran {
    failed: Vec<usize>,
    took: Vec<u64>,
}

impl Planned {
    pub(super) fn empty(budget: Budget) -> Self {
        Planned {
            jobs: Vec::new(),
            budget,
            generation: 0,
            last_sequence: 0,
            above: None,
            upto: None,
        }
    }

    fn query<'a>(&'a self, job: &Listed, lost: &'a dyn Fn(u64, u64, u64) -> bool) -> Query<'a> {
        Query {
            by_series: BySeries::new(job.by_series.clone()),
            ranges: job.ranges.clone(),
            generation: self.generation,
            last_sequence: self.last_sequence,
            lost,
            budget: &self.budget,
            above: self.above,
            upto: self.upto,
        }
    }

    /// Reads the planned segments' wanted batches into `out`, which is then in log order again; a segment whose
    /// lists or batches do not read as listed, or whose batches would take half its batch bytes or more, is left to
    /// [`LogReader::finish_planned`]. Nothing is read once `out` has overflowed.
    pub fn run(&self, out: &mut Lookup) -> Ran {
        let mut ran = Ran::default();
        if out.overflowed {
            return ran;
        }
        let start = out.facts.len();
        for (k, job) in self.jobs.iter().enumerate() {
            let before = out.facts.len();
            match self.read(job, &mut out.facts) {
                Some(()) => {
                    out.segments_listed += 1;
                    if out.facts.len() > before {
                        ran.took.push(job.name.file_id);
                    }
                }
                None => {
                    out.facts.truncate(before);
                    ran.failed.push(k);
                }
            }
            if self.budget.over() {
                out.overflowed = true;
                break;
            }
        }
        if out.facts.len() > start {
            out.facts.sort_by_key(|f| f.position);
        }
        ran
    }

    fn read(&self, job: &Listed, facts: &mut Vec<FoundFact>) -> Option<()> {
        let (at, len) = job.lists;
        let mut b = vec![0u8; usize::try_from(len).ok()?];
        if read_at_full(&*job.file, &mut b, at).ok()? != b.len() {
            return None;
        }
        let block = read_block(&b, 0, b.len() as u64).ok()?;
        if block.header.block_type != block_type::SERIES_BATCHES {
            return None;
        }
        let index_offset = job.batch_bytes + HEADER_LEN as u64;
        let (ids, refs) = SeriesBatches::wanted_in(block.body, index_offset, &job.ids)?;
        let bytes: u64 = refs.iter().map(|r| u64::from(r.length)).sum();
        if ids != job.index_ids || bytes.saturating_mul(2) >= job.batch_bytes {
            return None;
        }
        lookup_listed(
            &*job.file,
            &job.id,
            &refs,
            &self.query(job, &|_, _, _| false),
            facts,
        )
    }
}

impl LogReader {
    /// The read of a sealed segment through its series batches, when its index (cached) says it carries them.
    pub(super) fn listed_job(
        &self,
        name: SegmentName,
        candidates: &[usize],
        wanted: &[Wanted],
    ) -> Option<Listed> {
        #[cfg(test)]
        if self.caches.lists_off {
            return None;
        }
        if !name.sealed {
            return None;
        }
        let sealed = self.caches.index.get(&name.file_id)?.value.as_ref()?;
        let lists = sealed.lists?;
        // Facts stand in for bytes here: a read of most of the segment's facts reads it whole at once.
        let all: u64 = sealed.records.iter().map(|r| u64::from(r.fact_count)).sum();
        let wanted_facts: u64 = sealed
            .records
            .iter()
            .filter(|r| {
                candidates
                    .iter()
                    .any(|&i| wanted[i].series_id == r.series_id)
            })
            .map(|r| u64::from(r.fact_count))
            .sum();
        if wanted_facts.saturating_mul(2) >= all {
            return None;
        }
        let (n, file) = self.open_segment(name).ok().flatten()?;
        if !n.sealed {
            return None;
        }
        let mut by_series: BTreeMap<u32, Vec<(i64, i64)>> = BTreeMap::new();
        for &i in candidates {
            let w = &wanted[i];
            by_series
                .entry(w.series_id)
                .or_default()
                .push((w.from, w.to));
        }
        Some(Listed {
            name,
            id: SegmentIdentity {
                store_id: self.store_id,
                file_id: name.file_id,
                generation: n.generation,
            },
            file,
            lists,
            batch_bytes: sealed.batch_bytes,
            index_ids: sealed.records.iter().map(|r| r.series_id).collect(),
            candidates: candidates.to_vec(),
            ids: by_series.keys().copied().collect(),
            by_series,
            ranges: candidates
                .iter()
                .map(|&i| (wanted[i].from, wanted[i].to))
                .collect(),
        })
    }

    /// Ends a lookup after [`Planned::run`]: notes the segments it took facts from, and reads whole the segments it
    /// left, judging their regions with `lost` as [`LogReader::lookup_within`] does.
    pub fn finish_planned(
        &mut self,
        out: &mut Lookup,
        planned: &Planned,
        ran: Ran,
        lost: &dyn Fn(u64, u64, u64) -> bool,
    ) {
        self.caches.stood.extend(ran.took);
        if out.overflowed {
            return;
        }
        let start = out.facts.len();
        for k in ran.failed {
            let job = &planned.jobs[k];
            let before = out.facts.len();
            let query = planned.query(job, lost);
            let read = self.lookup_segment(job.name, ScanLimit::End, &query, &mut out.facts);
            if out.facts.len() > before {
                self.caches.stood.insert(job.name.file_id);
            }
            if !read {
                for &i in &job.candidates {
                    out.complete[i] = false;
                }
            }
            self.caches.trim();
            if planned.budget.over() {
                out.overflowed = true;
                break;
            }
        }
        if out.facts.len() > start {
            out.facts.sort_by_key(|f| f.position);
        }
    }
}

/// Reads only the batches `refs` (ascending, apart) of a sealed segment for `query`, those close together in one read
/// of at most [`PIECE`] bytes unless one batch is longer; `None` when one does not read as the complete batch listed,
/// so that the caller reads the segment whole and judges its regions (A.8a).
fn lookup_listed(
    f: &dyn crate::fs::FsFile,
    id: &SegmentIdentity,
    refs: &[BatchRef],
    query: &Query<'_>,
    facts: &mut Vec<FoundFact>,
) -> Option<()> {
    let mut min_first = 0u64;
    let mut k = 0;
    while k < refs.len() {
        let start = u64::from(refs[k].offset);
        let mut end = refs[k].end();
        let mut j = k + 1;
        while j < refs.len()
            && u64::from(refs[j].offset) <= end + LISTED_GAP
            && refs[j].end() - start <= PIECE
        {
            end = refs[j].end();
            j += 1;
        }
        let mut buf = buffer(usize::try_from(end - start).ok()?);
        if read_at_full(f, &mut buf, start).ok()? != buf.len() {
            return None;
        }
        for r in &refs[k..j] {
            let (o, e) = (u64::from(r.offset), r.end());
            let BatchRead::Complete(b, _) =
                read_batch_in_unjudged(&buf, start, id, o, e, min_first, Refusal::AtOnce)
            else {
                return None;
            };
            if b.end() != e {
                return None;
            }
            if b.commit.generation == query.generation
                && b.commit.last_sequence > query.last_sequence
            {
                return Some(());
            }
            min_first = b.commit.last_sequence.saturating_add(1);
            query.take(&b, facts, None)?;
            if query.budget.over() {
                return Some(());
            }
        }
        k = j;
    }
    Some(())
}
