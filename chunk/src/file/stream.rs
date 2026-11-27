//! A series written a block at a time (Q-811): [`FileWriter::begin_series`], [`FileWriter::push`] in `(t, position)`
//! order, [`FileWriter::end_series`]. The bytes equal [`FileWriter::series`] on the same facts, so the chunk order of
//! B.1 (by first `t`, then least position) holds: a chunk is streamed once no later chunk can sort before it, and is
//! held whole only while that is undecided (Q-1530).

use std::collections::VecDeque;
use std::io::Write;

use super::Position;
use super::desc::Class;
use super::parts::SET_ASIDE;
use super::stats::Reach;
use super::write::{ChunkAcc, Fact, FileWriter, WriteError};

/// The open series of a [`FileWriter`].
pub(crate) struct SeriesStream {
    series_id: u32,
    reach: Reach,
    /// Set-aside chunks not yet written, in chunk order.
    aside: VecDeque<Vec<Fact>>,
    /// Whole chunks of facts held because a later chunk could still sort before them, in arrival order.
    held: Vec<Vec<Fact>>,
    /// Rows of the current chunk not yet in a block.
    rows: Vec<Fact>,
    /// The current chunk, once its place is decided and its first block written.
    acc: Option<ChunkAcc>,
    /// Rows taken into the current chunk.
    taken: usize,
    first_t: i64,
    varied: bool,
    last: Option<(i64, Position)>,
}

impl SeriesStream {
    /// Rows the stream holds in memory, for a caller that counts them against a cap.
    pub(crate) fn held_rows(&self) -> usize {
        self.rows.len()
            + self.held.iter().map(Vec::len).sum::<usize>()
            + self.aside.iter().map(Vec::len).sum::<usize>()
    }
}

fn content(what: &'static str) -> WriteError {
    WriteError::Content(what)
}

fn key(c: &[Fact]) -> (i64, Position) {
    (
        c[0].t,
        c.iter().map(|f| f.position).min().unwrap_or_default(),
    )
}

impl<W: Write> FileWriter<W> {
    /// Opens a series for [`Self::push`]; its set-aside facts, which are few (a restore's), come whole.
    pub fn begin_series(
        &mut self,
        series_id: u32,
        name: Option<&str>,
        reach: Reach,
        mut set_aside: Vec<Fact>,
    ) -> Result<(), WriteError> {
        self.series_head(series_id, name, reach, !set_aside.is_empty())?;
        if set_aside.iter().any(|f| f.series_id != series_id) {
            return Err(content("a fact of another series"));
        }
        set_aside.sort_unstable_by_key(|f| (f.t, f.position));
        let mut aside = Vec::new();
        while !set_aside.is_empty() {
            let rest = set_aside.split_off(set_aside.len().min(self.chunk_rows));
            aside.push(std::mem::replace(&mut set_aside, rest));
        }
        aside.sort_by_key(|c| key(c));
        let aside = VecDeque::from(aside);
        self.stream = Some(SeriesStream {
            series_id,
            reach,
            aside,
            held: Vec::new(),
            rows: Vec::new(),
            acc: None,
            taken: 0,
            first_t: 0,
            varied: false,
            last: None,
        });
        Ok(())
    }

    /// Adds the open series' next fact: `(t, position)` strictly ascending, `t` strictly ascending in a current file.
    pub fn push(&mut self, f: Fact) -> Result<(), WriteError> {
        let current = self.description.class == Class::Current;
        let chunk_rows = self.chunk_rows;
        let s = self
            .stream
            .as_mut()
            .ok_or(content("a fact with no open series"))?;
        if f.series_id != s.series_id {
            return Err(content("a fact of another series"));
        }
        if let Some((t, p)) = s.last {
            if (f.t, f.position) <= (t, p) {
                return Err(content("facts out of (t, position) order"));
            }
            if current && f.t == t {
                return Err(content("two facts of one key in a current file"));
            }
        }
        s.last = Some((f.t, f.position));
        if s.taken == chunk_rows {
            self.end_chunk()?;
        }
        let s = self.stream.as_mut().expect("open");
        if s.taken == 0 {
            s.first_t = f.t;
            s.varied = false;
            let bound = f.t;
            self.emit_before(Some(bound))?;
        }
        let s = self.stream.as_mut().expect("open");
        s.varied |= f.t != s.first_t;
        s.taken += 1;
        s.rows.push(f);
        self.try_commit()?;
        let block_rows = self.block_rows;
        let s = self.stream.as_mut().expect("open");
        if s.acc.is_some() && s.rows.len() == block_rows {
            self.flush_rows()?;
        }
        Ok(())
    }

    /// Closes the open series, writing what it still holds.
    pub fn end_series(&mut self) -> Result<(), WriteError> {
        if self.stream.is_none() {
            return Err(content("no open series"));
        }
        self.end_chunk()?;
        self.emit_before(None)?;
        self.stream = None;
        Ok(())
    }

    /// Rows the open series holds in memory.
    pub fn held_rows(&self) -> usize {
        self.stream.as_ref().map_or(0, SeriesStream::held_rows)
    }

    /// Starts streaming the current chunk once nothing can sort before it: a later row has a greater `t`, no
    /// set-aside chunk shares its first `t`, and no chunk is held.
    fn try_commit(&mut self) -> Result<(), WriteError> {
        let s = self.stream.as_ref().expect("open");
        if s.acc.is_some()
            || !s.varied
            || !s.held.is_empty()
            || s.aside.iter().any(|c| c[0].t == s.first_t)
        {
            return Ok(());
        }
        let (id, block_rows) = (s.series_id, self.block_rows);
        let acc = self.chunk_begin(id, 0);
        let s = self.stream.as_mut().expect("open");
        s.acc = Some(acc);
        while self.stream.as_ref().expect("open").rows.len() >= block_rows {
            self.flush_rows()?;
        }
        Ok(())
    }

    /// Writes the first block's worth of the current chunk's rows.
    fn flush_rows(&mut self) -> Result<(), WriteError> {
        let s = self.stream.as_mut().expect("open");
        let n = s.rows.len().min(self.block_rows);
        let rows: Vec<Fact> = s.rows.drain(..n).collect();
        let reach = s.reach;
        let mut acc = s.acc.take().expect("committed");
        let r = self.chunk_block(&mut acc, reach, &rows);
        self.stream.as_mut().expect("open").acc = Some(acc);
        r
    }

    /// Ends the current chunk: a streamed one is completed, an undecided one is held whole.
    fn end_chunk(&mut self) -> Result<(), WriteError> {
        let s = self.stream.as_mut().expect("open");
        if s.taken == 0 {
            return Ok(());
        }
        s.taken = 0;
        if s.acc.is_some() {
            while !self.stream.as_ref().expect("open").rows.is_empty() {
                self.flush_rows()?;
            }
            let acc = self
                .stream
                .as_mut()
                .expect("open")
                .acc
                .take()
                .expect("committed");
            return self.chunk_end(acc);
        }
        let rows = std::mem::take(&mut s.rows);
        s.held.push(rows);
        Ok(())
    }

    /// Writes, in chunk order, every held and set-aside chunk whose first `t` is below `bound` (all at `None`).
    fn emit_before(&mut self, bound: Option<i64>) -> Result<(), WriteError> {
        loop {
            let s = self.stream.as_mut().expect("open");
            let below = |c: &Vec<Fact>| bound.is_none_or(|b| c[0].t < b);
            let held = s
                .held
                .iter()
                .enumerate()
                .filter(|(_, c)| below(c))
                .min_by_key(|(_, c)| key(c))
                .map(|(i, c)| (i, key(c)));
            let aside = s.aside.front().filter(|c| below(c)).map(|c| key(c));
            let (rows, flags) = match (held, aside) {
                (Some((i, h)), a) if a.is_none_or(|a| h < a) => (s.held.remove(i), 0),
                (_, Some(_)) => (s.aside.pop_front().expect("front"), SET_ASIDE),
                _ => return Ok(()),
            };
            let (id, reach) = (s.series_id, s.reach);
            self.chunk(id, reach, flags, &rows)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::super::desc::{Description, Input, Level};
    use super::super::layout::Identity;
    use super::super::times::Batch;
    use super::super::write::Series;
    use super::*;

    const BLOCK: usize = 3;
    const CHUNK: usize = 6;

    fn writer(class: Class, positions: u64, aside: bool) -> FileWriter<Vec<u8>> {
        let d = Description {
            class,
            level: Level::L0,
            period_start: 0,
            period_len: 1_000,
            run_id: 9,
            run_count: 1,
            inputs: vec![Input {
                kind: 1,
                file_id: 1,
            }],
            name_range: None,
            positions: vec![(1, positions)],
            has_entries: false,
            has_set_aside: aside,
            complete: true,
        };
        let id = Identity {
            store_id: [3; 16],
            generation: 1,
            file_id: 9,
        };
        let batches = vec![Batch {
            generation: 1,
            first_sequence: 0,
            last_sequence: 999,
            received_at: 0,
            commit_time: 0,
        }];
        FileWriter::new(Vec::new(), id, 0, &d, batches)
            .unwrap()
            .with_rows(BLOCK, CHUNK)
    }

    /// Facts of series 1 at the given times, with distinct positions in a shuffled order.
    fn facts(ts: &[i64], order: &[u64]) -> Vec<Fact> {
        ts.iter()
            .zip(order)
            .map(|(&t, &p)| Fact::value(1, (1, p), t, 1, (t as u64) << 8 | p))
            .collect()
    }

    fn whole(class: Class, s: &Series, n: u64) -> Vec<u8> {
        let mut w = writer(class, n, !s.set_aside.is_empty());
        w.series(s.clone()).unwrap();
        w.finish().unwrap().0
    }

    fn streamed(class: Class, s: &Series, n: u64) -> (Vec<u8>, usize) {
        let mut w = writer(class, n, !s.set_aside.is_empty());
        let mut f = s.facts.clone();
        f.sort_by_key(|f| (f.t, f.position));
        w.begin_series(s.series_id, s.name.as_deref(), s.reach, s.set_aside.clone())
            .unwrap();
        let mut peak = 0;
        for x in f {
            w.push(x).unwrap();
            peak = peak.max(w.held_rows());
        }
        w.end_series().unwrap();
        (w.finish().unwrap().0, peak)
    }

    fn series(facts: Vec<Fact>, set_aside: Vec<Fact>) -> Series {
        Series {
            series_id: 1,
            name: Some("s".into()),
            reach: Reach::Point,
            facts,
            set_aside,
        }
    }

    fn split(ts: Vec<i64>, aside_mask: Vec<bool>, seed: u64) -> (Series, u64) {
        let n = ts.len() as u64;
        let mut order: Vec<u64> = (0..n).collect();
        let mut x = seed | 1;
        for i in (1..order.len()).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            order.swap(i, (x % (i as u64 + 1)) as usize);
        }
        let all = facts(&ts, &order);
        let (aside, kept): (Vec<_>, Vec<_>) = all
            .into_iter()
            .zip(aside_mask.iter().cycle())
            .partition(|(_, a)| **a);
        (
            series(
                kept.into_iter().map(|x| x.0).collect(),
                aside.into_iter().map(|x| x.0).collect(),
            ),
            n,
        )
    }

    proptest! {
        #[test]
        fn a_streamed_revision_series_writes_the_bytes_of_the_whole_one(
            ts in proptest::collection::vec(0i64..12, 1..40),
            mask in proptest::collection::vec(proptest::bool::weighted(0.2), 1..8),
            seed in any::<u64>(),
        ) {
            let (s, n) = split(ts, mask, seed);
            prop_assert!(streamed(Class::Revision, &s, n).0 == whole(Class::Revision, &s, n), "{:?}", s);
        }

        #[test]
        fn a_streamed_current_series_holds_at_most_a_block_and_writes_the_same_bytes(
            mut ts in proptest::collection::btree_set(0i64..400, 1..60).prop_map(|s| s.into_iter().collect::<Vec<_>>()),
            seed in any::<u64>(),
        ) {
            ts.reverse();
            let (s, n) = split(ts, vec![false], seed);
            let (bytes, peak) = streamed(Class::Current, &s, n);
            prop_assert!(bytes == whole(Class::Current, &s, n));
            prop_assert!(peak <= BLOCK, "held {peak} rows");
        }
    }

    #[test]
    fn a_chunk_boundary_inside_one_key_orders_chunks_by_least_position() {
        let ts = vec![5; 2 * CHUNK + 1];
        let order: Vec<u64> = (0..ts.len() as u64).rev().collect();
        let s = series(facts(&ts, &order), Vec::new());
        let n = ts.len() as u64;
        assert_eq!(
            streamed(Class::Revision, &s, n).0,
            whole(Class::Revision, &s, n)
        );
    }

    #[test]
    fn a_set_aside_chunk_sharing_the_first_time_is_ordered_by_least_position() {
        let ts = [4, 4, 4, 6, 7, 8, 9, 10];
        let order = [5, 1, 7, 0, 2, 3, 4, 6];
        let all = facts(&ts, &order);
        let s = series(
            all.iter().filter(|f| f.position.1 != 1).cloned().collect(),
            all.iter().filter(|f| f.position.1 == 1).cloned().collect(),
        );
        assert_eq!(
            streamed(Class::Revision, &s, 8).0,
            whole(Class::Revision, &s, 8)
        );
    }

    #[test]
    fn a_fact_out_of_order_or_repeating_a_current_key_is_refused() {
        let mut w = writer(Class::Current, 3, false);
        w.begin_series(1, Some("s"), Reach::Point, Vec::new())
            .unwrap();
        w.push(Fact::value(1, (1, 1), 5, 1, 0)).unwrap();
        assert!(w.push(Fact::value(1, (1, 0), 5, 1, 0)).is_err());
        assert!(w.push(Fact::value(1, (1, 2), 5, 1, 0)).is_err());
        assert!(w.push(Fact::value(2, (1, 2), 6, 1, 0)).is_err());
        assert!(w.series(series(Vec::new(), Vec::new())).is_err());
        assert!(w.entries(&[]).is_err());
    }
}
