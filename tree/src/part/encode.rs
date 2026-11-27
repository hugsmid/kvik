use kvik_log::block::encode_block;
use kvik_log::crc::crc32c;
use kvik_log::writer::Position;

use super::{
    ARRAY_PREFIX, DIRECTORY_PER_BLOCK, DIRECTORY_RECORD, DIRECTORY_ROOT_RECORD, END_MARKER, Entry,
    KIND, Part, Run, TARGET_BODY, block_type,
};
use crate::frame::{
    EncodeError, LEVEL_RECORD, check_header_fields, put_levels, put_position, put_uv, width_of,
};
use crate::level::MINUTE;

/// Bytes of a part and where its regions start.
pub(super) struct Layout {
    pub bytes: Vec<u8>,
    pub directory_at: usize,
    pub root_at: usize,
    pub trailer_at: usize,
}

struct NodesBlock {
    series: u64,
    level: u8,
    entries: u32,
    first_start: i64,
    last_end: i64,
    body: Vec<u8>,
}

pub(super) fn encode(p: &Part) -> Result<Vec<u8>, EncodeError> {
    Ok(layout(p)?.bytes)
}

pub(super) fn layout(p: &Part) -> Result<Layout, EncodeError> {
    check_header_fields(&p.header, KIND)?;
    let mut out = p.header.encode().to_vec();
    let mut directory: Vec<[u8; DIRECTORY_RECORD]> = Vec::new();
    let mut least_start = i64::MAX;
    let mut greatest_end = i64::MIN;
    let mut entry_count = 0u64;
    for run in &p.runs {
        let width = width_of(&p.levels, run.level).ok_or(EncodeError::Shape(
            "a run's level is not in the level table",
        ))?;
        for b in blocks(run, width)? {
            let at = out.len() as u64;
            let h = encode_block(block_type::NODES, 0, &b.body, &mut out)
                .map_err(|_| EncodeError::Shape("a nodes block body above 16 MiB"))?;
            let mut r = [0u8; DIRECTORY_RECORD];
            r[0..8].copy_from_slice(&b.series.to_le_bytes());
            r[8] = b.level;
            r[12..16].copy_from_slice(&b.entries.to_le_bytes());
            r[16..24].copy_from_slice(&b.first_start.to_le_bytes());
            r[24..32].copy_from_slice(&b.last_end.to_le_bytes());
            r[32..40].copy_from_slice(&at.to_le_bytes());
            r[40..44].copy_from_slice(&(h.block_len() as u32).to_le_bytes());
            directory.push(r);
            least_start = least_start.min(b.first_start);
            greatest_end = greatest_end.max(b.last_end);
            entry_count += u64::from(b.entries);
        }
    }
    let directory_at = out.len();
    let mut roots: Vec<[u8; DIRECTORY_ROOT_RECORD]> = Vec::new();
    for chunk in directory.chunks(DIRECTORY_PER_BLOCK) {
        let at = out.len() as u64;
        let body = array(
            chunk.len(),
            DIRECTORY_RECORD,
            chunk.iter().flatten().copied(),
        );
        encode_block(block_type::DIRECTORY, 0, &body, &mut out).expect("directory body is small");
        let first = &chunk[0];
        let mut r = [0u8; DIRECTORY_ROOT_RECORD];
        r[0..9].copy_from_slice(&first[0..9]);
        r[12..16].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
        r[16..24].copy_from_slice(&first[16..24]);
        r[24..32].copy_from_slice(&at.to_le_bytes());
        roots.push(r);
    }
    let root_at = out.len();
    let body = array(
        roots.len(),
        DIRECTORY_ROOT_RECORD,
        roots.iter().flatten().copied(),
    );
    encode_block(block_type::DIRECTORY_ROOT, 0, &body, &mut out)
        .map_err(|_| EncodeError::Shape("a directory root above 16 MiB"))?;
    let trailer_at = out.len();
    let (series_lo, series_hi) = match (p.runs.first(), p.runs.last()) {
        (Some(a), Some(b)) => (a.series, b.series),
        _ => (0, 0),
    };
    if entry_count == 0 {
        least_start = 0;
        greatest_end = 0;
    }
    let mut t = Vec::with_capacity(super::TRAILER_FIXED + LEVEL_RECORD * p.levels.len());
    t.extend_from_slice(&p.seq.to_le_bytes());
    t.push(p.class as u8);
    t.push(0);
    let levels =
        u16::try_from(p.levels.len()).map_err(|_| EncodeError::Shape("more than 65,535 levels"))?;
    t.extend_from_slice(&levels.to_le_bytes());
    t.extend_from_slice(&(LEVEL_RECORD as u16).to_le_bytes());
    t.extend_from_slice(&[0; 2]);
    t.extend_from_slice(&p.period_start.to_le_bytes());
    t.extend_from_slice(&p.period_len.to_le_bytes());
    t.extend_from_slice(&least_start.to_le_bytes());
    t.extend_from_slice(&greatest_end.to_le_bytes());
    t.extend_from_slice(&series_lo.to_le_bytes());
    t.extend_from_slice(&series_hi.to_le_bytes());
    put_position(&mut t, p.through);
    t.extend_from_slice(&[0; 4]);
    t.extend_from_slice(&entry_count.to_le_bytes());
    t.extend_from_slice(&(root_at as u64).to_le_bytes());
    t.extend_from_slice(&crc32c(&out).to_le_bytes());
    t.extend_from_slice(&[0; 4]);
    put_levels(&mut t, &p.levels);
    encode_block(block_type::TRAILER, 0, &t, &mut out)
        .map_err(|_| EncodeError::Shape("a trailer above 16 MiB"))?;
    out.extend_from_slice(&(trailer_at as u64).to_le_bytes());
    out.extend_from_slice(END_MARKER);
    Ok(Layout {
        bytes: out,
        directory_at,
        root_at,
        trailer_at,
    })
}

fn array(count: usize, size: usize, records: impl Iterator<Item = u8>) -> Vec<u8> {
    let mut b = Vec::with_capacity(ARRAY_PREFIX + count * size);
    b.extend_from_slice(&(count as u32).to_le_bytes());
    b.extend_from_slice(&(size as u16).to_le_bytes());
    b.extend_from_slice(&[0; 2]);
    b.extend(records);
    b
}

/// The least `as_of` of a run: every block of the run carries it as `base_pos` (T.9).
pub(super) fn base_of(run: &Run) -> Position {
    run.entries
        .iter()
        .map(|e| e.as_of)
        .min()
        .unwrap_or_default()
}

/// Entry bytes after `prev_end` (or as a block's first entry when `None`), `as_of` relative to `base`.
pub(super) fn entry(
    e: &Entry,
    prev_end: Option<i64>,
    base: Position,
    node: &[u8],
    w: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    let gap = match prev_end {
        None => 0,
        Some(end) => {
            let d = e
                .start
                .checked_sub(end)
                .filter(|d| *d >= 0 && d % MINUTE == 0)
                .ok_or(EncodeError::Shape("entries not ascending by whole minutes"))?;
            (d / MINUTE) as u64
        }
    };
    put_uv(w, gap);
    w.push(u8::from(e.stale));
    let dg = e
        .as_of
        .generation
        .checked_sub(base.generation)
        .ok_or(EncodeError::Shape("as_of below base"))?;
    put_uv(w, u64::from(dg));
    let seq = if dg == 0 {
        e.as_of
            .sequence
            .checked_sub(base.sequence)
            .ok_or(EncodeError::Shape("as_of below base"))?
    } else {
        e.as_of.sequence
    };
    put_uv(w, seq);
    put_uv(w, node.len() as u64);
    w.extend_from_slice(node);
    Ok(())
}

fn head(series: u64, level: u8, start: i64, base: Position) -> Vec<u8> {
    let mut b = Vec::with_capacity(TARGET_BODY.min(4096));
    put_uv(&mut b, series);
    b.push(level);
    b.extend_from_slice(&start.to_le_bytes());
    put_position(&mut b, base);
    b
}

/// A run split as the writer splits it (T.9): greedily, a block taking entries while its body stays within
/// [`TARGET_BODY`], and always at least one.
fn blocks(run: &Run, width: i64) -> Result<Vec<NodesBlock>, EncodeError> {
    let base = base_of(run);
    let mut out: Vec<NodesBlock> = Vec::new();
    let mut cur: Option<NodesBlock> = None;
    let mut scratch = Vec::new();
    for e in &run.entries {
        let node = match &e.node {
            Some(n) => crate::encode(n)?,
            None => Vec::new(),
        };
        let end = e
            .start
            .checked_add(width)
            .ok_or(EncodeError::Shape("an entry end beyond i64"))?;
        if let Some(b) = cur.as_mut() {
            scratch.clear();
            entry(e, Some(b.last_end), base, &node, &mut scratch)?;
            if b.body.len() + scratch.len() <= TARGET_BODY {
                b.body.extend_from_slice(&scratch);
                b.entries += 1;
                b.last_end = end;
                continue;
            }
            out.push(cur.take().expect("open block"));
        }
        let mut body = head(run.series, run.level, e.start, base);
        entry(e, None, base, &node, &mut body)?;
        cur = Some(NodesBlock {
            series: run.series,
            level: run.level,
            entries: 1,
            first_start: e.start,
            last_end: end,
            body,
        });
    }
    out.extend(cur);
    Ok(out)
}
