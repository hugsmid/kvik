use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::lookup_properties::{World, seal_lists};
use super::*;
use crate::block::{BLOCK_HEADER_LEN, block_type, encode_block, read_block};
use crate::body::{END_MARKER_LEN, TRAILER_LEN, Trailer, decode_end_marker};
use crate::fs::{DirEntry, Fs};
use crate::series_batches::{BatchRef, SeriesBatches};

#[derive(Debug, Default)]
struct Open {
    now: AtomicUsize,
    peak: AtomicUsize,
    /// Bytes read from sealed segments, and the longest single read.
    bytes: AtomicUsize,
    longest: AtomicUsize,
}

struct CountingFs {
    inner: Arc<MemFs>,
    open: Arc<Open>,
}

impl fmt::Debug for CountingFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CountingFs")
    }
}

#[derive(Debug)]
struct Counted {
    inner: Box<dyn FsFile>,
    open: Option<Arc<Open>>,
    sealed: bool,
}

impl Drop for Counted {
    fn drop(&mut self) {
        if let Some(o) = &self.open {
            o.now.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl FsFile for Counted {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        if let (Some(o), true) = (&self.open, self.sealed) {
            o.bytes.fetch_add(buf.len(), Ordering::SeqCst);
            o.longest.fetch_max(buf.len(), Ordering::SeqCst);
        }
        self.inner.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        self.inner.write_at(buf, offset)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        self.inner.sync_data()
    }
    fn sync_all(&self) -> std::io::Result<()> {
        self.inner.sync_all()
    }
    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }
    fn try_lock_exclusive(&self) -> std::io::Result<bool> {
        self.inner.try_lock_exclusive()
    }
    fn set_len(&self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }
}

impl Fs for CountingFs {
    fn open(&self, path: &Path, mode: OpenMode) -> std::io::Result<Box<dyn FsFile>> {
        let inner = self.inner.open(path, mode)?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(SegmentName::parse);
        let sealed = name.is_some_and(|n| n.sealed);
        let open = name.map(|_| self.open.clone());
        if let Some(o) = &open {
            let now = o.now.fetch_add(1, Ordering::SeqCst) + 1;
            o.peak.fetch_max(now, Ordering::SeqCst);
        }
        Ok(Box::new(Counted {
            inner,
            open,
            sealed,
        }))
    }
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn read_dir(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
        self.inner.read_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.inner.remove_file(path)
    }
    fn sync_dir(&self, path: &Path) -> std::io::Result<()> {
        self.inner.sync_dir(path)
    }
    fn free_space(&self, path: &Path) -> std::io::Result<u64> {
        self.inner.free_space(path)
    }
}

fn series(series_id: u32) -> [Wanted; 1] {
    [Wanted {
        series_id,
        from: i64::MIN,
        to: i64::MAX,
    }]
}

#[test]
fn a_listed_lookup_holds_one_segment_file_open_at_a_time_however_many_segments_it_spans() {
    let mut w = World::new();
    while w.sealed().len() < 8 {
        w.call(1);
    }
    let open = Arc::new(Open::default());
    let mut r = w.reader_on(Arc::new(CountingFs {
        inner: w.fs.clone(),
        open: open.clone(),
    }));
    open.peak.store(0, Ordering::SeqCst);
    let got = r.lookup(&series(1), &|_, _, _| false).unwrap();
    assert!(got.segments_listed >= 6, "listed {}", got.segments_listed);
    assert_eq!(got.complete, [true]);
    let peak = open.peak.load(Ordering::SeqCst);
    assert!(peak <= 2, "{peak} segment files open at once");
}

fn counting(w: &mut World) -> (LogReader, Arc<Open>) {
    let open = Arc::new(Open::default());
    let r = w.reader_on(Arc::new(CountingFs {
        inner: w.fs.clone(),
        open: open.clone(),
    }));
    (r, open)
}

fn listed_world(sealed: usize) -> World {
    let mut w = World::new();
    while w.sealed().len() < sealed {
        w.call(1);
    }
    w
}

/// Where a sealed segment's index block, series batches block and index offset lie.
struct Seal {
    index: (usize, usize),
    lists: (usize, usize),
    index_offset: u64,
}

fn seal_of(w: &World, name: SegmentName) -> Seal {
    let bytes = w.fs.read(&w.path(name)).unwrap();
    let len = bytes.len() as u64;
    let at = decode_end_marker(&bytes[bytes.len() - END_MARKER_LEN..]).unwrap();
    let trailer = Trailer::decode(read_block(&bytes, at, len).unwrap().body).unwrap();
    let index = read_block(&bytes, trailer.index_offset, at).unwrap();
    let lists = read_block(&bytes, index.next_offset(), at).unwrap();
    assert_eq!(lists.header.block_type, block_type::SERIES_BATCHES);
    Seal {
        index: (index.offset as usize, index.header.block_len() as usize),
        lists: (lists.offset as usize, lists.header.block_len() as usize),
        index_offset: trailer.index_offset,
    }
}

/// Rewrites the lists of a sealed segment in place, by a body of the same length, its checksums valid.
fn rewrite_lists(w: &World, name: SegmentName, edit: impl FnOnce(&mut SeriesBatches, u64)) {
    let seal = seal_of(w, name);
    let (_, lists) = seal_lists(&w.fs.read(&w.path(name)).unwrap());
    let mut lists = lists.unwrap();
    edit(&mut lists, seal.index_offset);
    let mut block = Vec::new();
    encode_block(block_type::SERIES_BATCHES, 1, &lists.encode(), &mut block).unwrap();
    assert_eq!(block.len(), seal.lists.1);
    w.fs.poke(&w.path(name), seal.lists.0, &block);
}

/// Looks `wanted` up with the lists and whole; the segments the first read through them.
fn both(w: &mut World, wanted: &[Wanted]) -> (Lookup, Lookup) {
    let got = w.reader().lookup(wanted, &|_, _, _| false).unwrap();
    let mut whole = w.reader();
    whole.caches.lists_off();
    (got, whole.lookup(wanted, &|_, _, _| false).unwrap())
}

/// The lists of the first sealed segment, once `edit` has changed them, are refused: that segment is read whole, and
/// nothing is lost, added twice or put out of order.
fn refused(edit: impl FnOnce(&mut SeriesBatches, u64)) {
    let mut w = listed_world(3);
    let wanted = series(1);
    let (before, _) = both(&mut w, &wanted);
    assert!(before.segments_listed >= 2);
    rewrite_lists(&w, w.sealed()[0], edit);
    let (got, whole) = both(&mut w, &wanted);
    assert_eq!(got.segments_listed, before.segments_listed - 1);
    assert_eq!(got.facts, whole.facts);
    assert_eq!(got.complete, whole.complete);
    assert!(got.facts.windows(2).all(|p| p[0].position < p[1].position));
}

#[test]
fn lists_naming_other_series_than_the_index_are_refused() {
    refused(|l, _| l.series.last_mut().unwrap().0 += 50);
}

#[test]
fn a_reference_longer_than_its_batch_is_refused() {
    refused(|l, index_offset| {
        let (_, start, n) = *l.series.iter().find(|s| s.0 == 1).unwrap();
        let own = start as usize..(start + n) as usize;
        let k = own
            .clone()
            .find(|&k| {
                let end = l.refs[k].end() + 8;
                end <= index_offset && (k + 1 == own.end || end <= u64::from(l.refs[k + 1].offset))
            })
            .unwrap();
        l.refs[k].length += 8;
    });
}

#[test]
fn batches_listed_out_of_sequence_order_are_refused() {
    let mut w = listed_world(3);
    let wanted = series(1);
    let (before, _) = both(&mut w, &wanted);
    let name = w.sealed()[0];
    let (_, lists) = seal_lists(&w.fs.read(&w.path(name)).unwrap());
    let refs = lists.unwrap().of(1).unwrap().to_vec();
    let (a, b) = (refs[0], refs[1]);
    assert_eq!(a.length, b.length);
    let bytes = w.fs.read(&w.path(name)).unwrap();
    let take = |r: BatchRef| bytes[r.offset as usize..r.end() as usize].to_vec();
    w.fs.poke(&w.path(name), a.offset as usize, &take(b));
    w.fs.poke(&w.path(name), b.offset as usize, &take(a));
    let (got, whole) = both(&mut w, &wanted);
    assert_eq!(got.segments_listed, before.segments_listed - 1);
    assert_eq!(got.facts, whole.facts);
    assert_eq!(got.complete, whole.complete);
}

#[test]
fn a_lookup_of_two_series_sharing_batches_finds_each_fact_once_in_log_order() {
    let mut w = World::new();
    w.series = 12;
    while w.sealed().len() < 3 {
        w.call(2);
    }
    let wanted = [series(1)[0], series(2)[0]];
    let (got, whole) = both(&mut w, &wanted);
    assert!(got.segments_listed >= 2);
    assert_eq!(got.facts, whole.facts);
    assert!(got.facts.windows(2).all(|p| p[0].position < p[1].position));
}

#[test]
fn a_failed_listed_read_leaves_its_facts_to_the_whole_read_alone() {
    let mut w = listed_world(3);
    let wanted = series(1);
    let name = w.sealed()[1];
    let (_, lists) = seal_lists(&w.fs.read(&w.path(name)).unwrap());
    let refs = lists.unwrap().of(1).unwrap().to_vec();
    let last = refs.last().unwrap();
    let at = last.offset as usize + 20;
    let b = w.fs.read(&w.path(name)).unwrap()[at];
    w.fs.poke(&w.path(name), at, &[b ^ 0x5A]);
    let (got, whole) = both(&mut w, &wanted);
    assert_eq!(got.facts, whole.facts);
    assert_eq!(got.complete, [false]);
}

#[test]
fn a_listed_read_takes_nothing_above_the_mark_it_was_planned_under() {
    let mut w = listed_world(3);
    let wanted = series(1);
    let mut r = w.reader();
    let (mut out, mut planned) = r
        .lookup_planned(&wanted, &|_, _, _| false, Within::default())
        .unwrap();
    let name = w.sealed()[1];
    let (first, last) = w.sequences(name);
    let mark = first + (last - first) / 2;
    planned.behind(mark);
    let ran = planned.run(&mut out);
    r.finish_planned(&mut out, &planned, ran, &|_, _, _| false);
    let above = out
        .facts
        .iter()
        .filter(|f| f.position.sequence > mark)
        .count();
    let listed_above = out
        .facts
        .iter()
        .filter(|f| f.position.sequence > mark && f.position.sequence <= last)
        .count();
    assert!(out.facts.iter().any(|f| f.position.sequence <= mark));
    assert_eq!(listed_above, 0, "{above} facts above the mark");
}

#[test]
fn a_segment_taken_from_only_through_its_lists_and_then_gone_makes_later_lookups_incomplete() {
    let mut w = listed_world(3);
    let wanted = series(1);
    let mut r = w.reader();
    let first = r.lookup(&wanted, &|_, _, _| false).unwrap();
    assert!(first.segments_listed >= 2 && first.complete == [true]);
    w.fs.remove_file(&w.path(w.sealed()[0])).unwrap();
    let again = r.lookup(&wanted, &|_, _, _| false).unwrap();
    assert_eq!(again.complete, [false]);
}

#[test]
fn planning_a_lookup_reads_each_sealed_segments_index_but_not_its_lists() {
    let mut w = listed_world(3);
    let (mut r, open) = counting(&mut w);
    r.lookup_planned(&series(1), &|_, _, _| false, Within::default())
        .unwrap();
    let tail = BLOCK_HEADER_LEN + TRAILER_LEN + END_MARKER_LEN;
    let own: usize = w
        .sealed()
        .iter()
        .map(|n| {
            let s = seal_of(&w, *n);
            tail + BLOCK_HEADER_LEN + s.index.1 + BLOCK_HEADER_LEN
        })
        .sum();
    let read = open.bytes.load(Ordering::SeqCst);
    assert!(read <= own, "read {read}, index share {own}");
}

#[test]
fn lists_longer_than_a_piece_are_not_read_and_the_segment_is_read_whole() {
    let mut w = listed_world(3);
    let wanted = series(1);
    let mut r = w.reader();
    r.caches.limit(8 << 20, 512);
    let got = r.lookup(&wanted, &|_, _, _| false).unwrap();
    assert!(w.sealed().iter().all(|n| seal_of(&w, *n).lists.1 > 512));
    assert_eq!(got.segments_listed, 0);
    let (_, whole) = both(&mut w, &wanted);
    assert_eq!(got.facts, whole.facts);
}

#[test]
fn batches_listed_close_together_are_read_in_pieces_of_at_most_the_piece() {
    let mut w = listed_world(3);
    let piece = 1_024u64;
    let (mut r, open) = counting(&mut w);
    r.caches.limit(8 << 20, piece);
    let (mut out, planned) = r
        .lookup_planned(&series(1), &|_, _, _| false, Within::default())
        .unwrap();
    let longest = w
        .sealed()
        .iter()
        .flat_map(|n| seal_lists(&w.fs.read(&w.path(*n)).unwrap()).1.unwrap().refs)
        .map(|r| u64::from(r.length))
        .max()
        .unwrap();
    open.longest.store(0, Ordering::SeqCst);
    let ran = planned.run(&mut out);
    r.finish_planned(&mut out, &planned, ran, &|_, _, _| false);
    assert!(out.segments_listed >= 2);
    let read = open.longest.load(Ordering::SeqCst) as u64;
    let lists = w
        .sealed()
        .iter()
        .map(|n| seal_of(&w, *n).lists.1 as u64)
        .max()
        .unwrap();
    assert!(read <= piece.max(longest).max(lists), "read {read} at once");
}
