use kvik_log::crc::crc32c;
use kvik_log::writer::Position;

use super::encode::{base_of, layout};
use super::{
    Class, DIRECTORY_RECORD, DIRECTORY_ROOT_RECORD, END_MARKER, Entry, KIND, KNOWN, Part, Run,
    TRAILER_FIXED, block_type,
};
use crate::bytes::DecodeError;
use crate::frame::{
    Block, Cursor, FileError, LEVEL_RECORD, Refusal, block, expect_type, fail, header, header_len,
};
use crate::level::{Level, MINUTE};
use crate::node::Kind;

pub(super) fn decode(bytes: &[u8]) -> Result<Part, FileError> {
    let h = header(bytes, KIND)?;
    let len = bytes.len() as u64;
    if len < header_len() + 16 || &bytes[bytes.len() - 8..] != END_MARKER {
        return fail(len.saturating_sub(8), Refusal::EndMarker);
    }
    let tail = len - 16;
    let trailer_at = u64::from_le_bytes(
        bytes[tail as usize..tail as usize + 8]
            .try_into()
            .expect("8"),
    );
    if trailer_at < header_len() || trailer_at % 8 != 0 || trailer_at >= tail {
        return fail(tail, Refusal::TrailerOffset);
    }
    let t = block(bytes, trailer_at, tail)?;
    if t.next != tail {
        return fail(tail, Refusal::TrailerOffset);
    }
    expect_type(&t, block_type::TRAILER, KNOWN)?;
    let tr = trailer(t.body, trailer_at + 16)?;
    if crc32c(&bytes[..trailer_at as usize]) != tr.file_crc {
        return fail(trailer_at + 16 + 96, Refusal::FileChecksum);
    }
    let period_end = tr.period_start + tr.period_len as i64;

    let mut runs: Vec<Run> = Vec::new();
    let mut bases: Vec<(u64, Position)> = Vec::new();
    let mut kind: Option<Kind> = None;
    let mut directory_at = None;
    let mut root_at = None;
    let mut at = header_len();
    while at < trailer_at {
        let b = block(bytes, at, trailer_at)?;
        match b.block_type {
            block_type::NODES if directory_at.is_none() && root_at.is_none() => {
                let n = nodes(b.body, b.at + 16, &tr.levels)?;
                for e in &n.entries {
                    let end = e.start + width(&tr.levels, n.level);
                    if e.start < tr.period_start || end > period_end {
                        return fail(b.at, Refusal::OutsidePeriod);
                    }
                }
                match runs.last_mut() {
                    Some(r) if (r.series, r.level) == (n.series, n.level) => {
                        let last = r.entries.last().expect("runs are not empty").start;
                        if n.entries[0].start < last + width(&tr.levels, r.level) {
                            return fail(b.at, Refusal::Order);
                        }
                        kind_matches(&mut kind, &n.entries, b.at)?;
                        r.entries.extend(n.entries);
                    }
                    last => {
                        if last
                            .as_ref()
                            .is_some_and(|r| (r.series, r.level) >= (n.series, n.level))
                        {
                            return fail(b.at, Refusal::Order);
                        }
                        // T.9: one form per series, across its levels.
                        if last.is_none_or(|r| r.series != n.series) {
                            kind = None;
                        }
                        kind_matches(&mut kind, &n.entries, b.at)?;
                        if let Some(r) = runs.last() {
                            check_bases(r, &bases)?;
                        }
                        bases.clear();
                        runs.push(Run {
                            series: n.series,
                            level: n.level,
                            entries: n.entries,
                        });
                    }
                }
                bases.push((b.at, n.base));
            }
            block_type::DIRECTORY if root_at.is_none() => {
                array_record_size(&b, DIRECTORY_RECORD)?;
                directory_at.get_or_insert(b.at);
            }
            block_type::DIRECTORY_ROOT if root_at.is_none() => {
                array_record_size(&b, DIRECTORY_ROOT_RECORD)?;
                root_at = Some(b.at);
            }
            _ => expect_type(&b, 0, KNOWN)?,
        }
        at = b.next;
    }
    if let Some(r) = runs.last() {
        check_bases(r, &bases)?;
    }
    let Some(root_at) = root_at else {
        return fail(trailer_at, Refusal::BlockOrder);
    };
    let directory_at = directory_at.unwrap_or(root_at);

    let part = Part {
        header: h,
        seq: tr.seq,
        class: tr.class,
        period_start: tr.period_start,
        period_len: tr.period_len,
        through: tr.through,
        levels: tr.levels,
        runs,
    };
    let want = layout(&part).map_err(|_| FileError {
        at: header_len(),
        refusal: Refusal::Split,
    })?;
    let w = &want.bytes;
    let regions = [
        (
            header_len() as usize,
            directory_at as usize,
            want.directory_at,
            Refusal::Split,
        ),
        (
            directory_at as usize,
            root_at as usize,
            want.root_at,
            Refusal::Directory,
        ),
        (
            root_at as usize,
            trailer_at as usize,
            want.trailer_at,
            Refusal::DirectoryRoot,
        ),
        (trailer_at as usize, bytes.len(), w.len(), Refusal::Summary),
    ];
    let mut wstart = header_len() as usize;
    for (start, end, wend, refusal) in regions {
        let have = &bytes[start..end];
        let need = &w[wstart..wend];
        if have != need {
            let i = have.iter().zip(need).take_while(|(a, b)| a == b).count();
            return fail((start + i) as u64, refusal);
        }
        wstart = wend;
    }
    Ok(part)
}

fn width(levels: &[Level], id: u8) -> i64 {
    crate::frame::width_of(levels, id).expect("level checked when its block was read")
}

fn kind_matches(kind: &mut Option<Kind>, es: &[Entry], at: u64) -> Result<(), FileError> {
    for n in es.iter().filter_map(|e| e.node.as_ref()) {
        match *kind {
            Some(k) if n.body.kind() != k => return fail(at, Refusal::NodeKind),
            _ => *kind = Some(n.body.kind()),
        }
    }
    Ok(())
}

fn check_bases(r: &Run, bases: &[(u64, Position)]) -> Result<(), FileError> {
    let least = base_of(r);
    match bases.iter().find(|(_, b)| *b != least) {
        Some((at, _)) => fail(at + 16, Refusal::BasePosition),
        None => Ok(()),
    }
}

struct Trailer {
    seq: u64,
    class: Class,
    period_start: i64,
    period_len: u64,
    through: Position,
    file_crc: u32,
    levels: Vec<Level>,
}

/// The trailer's own fields (T.11); the summary fields are compared with a re-encoding by the caller.
fn trailer(body: &[u8], base: u64) -> Result<Trailer, FileError> {
    let mut c = Cursor::new(body, base);
    let seq = c.u64()?;
    let class_at = c.at();
    let class = c.u8()?;
    c.reserved(1)?;
    let count = c.u16()? as usize;
    let size_at = c.at();
    let size = c.u16()? as usize;
    c.reserved(2)?;
    let period_at = c.at();
    let period_start = c.i64()?;
    let period_len = c.u64()?;
    c.take(32)?;
    let through = c.position()?;
    c.reserved(4)?;
    c.take(16)?;
    let file_crc = c.u32()?;
    c.reserved(4)?;
    debug_assert_eq!(c.pos, TRAILER_FIXED);
    record_size(size, LEVEL_RECORD, size_at)?;
    if body.len() != TRAILER_FIXED + count * size {
        return fail(c.at(), Refusal::ArrayLength);
    }
    let class = Class::from_u8(class).ok_or(FileError {
        at: class_at,
        refusal: Refusal::UnknownClass,
    })?;
    let ok = i64::try_from(period_len)
        .ok()
        .filter(|l| *l > 0)
        .and_then(|l| period_start.checked_add(l));
    if ok.is_none() {
        return fail(period_at, Refusal::Period);
    }
    let levels = crate::frame::levels(&mut c, count)?;
    Ok(Trailer {
        seq,
        class,
        period_start,
        period_len,
        through,
        file_crc,
        levels,
    })
}

/// The `record_size` of a directory or directory root body (T.10), read before the body is compared with a
/// re-encoding so that a newer writer's larger records take a version refusal (Q-1462).
fn array_record_size(b: &Block<'_>, v1: usize) -> Result<(), FileError> {
    let mut c = Cursor::new(b.body, b.at + 16);
    c.u32()?;
    let at = c.at();
    record_size(c.u16()? as usize, v1, at)
}

/// A record size: below `v1` is damage, above it a newer version's records (`format.md` A.3).
pub(crate) fn record_size(size: usize, v1: usize, at: u64) -> Result<(), FileError> {
    if size < v1 {
        fail(at, Refusal::RecordSizeSmall)
    } else if size > v1 {
        fail(at, Refusal::RecordSize)
    } else {
        Ok(())
    }
}

struct Nodes {
    series: u64,
    level: u8,
    base: Position,
    entries: Vec<Entry>,
}

/// One nodes block body (T.9).
fn nodes(body: &[u8], base_at: u64, levels: &[Level]) -> Result<Nodes, FileError> {
    let mut c = Cursor::new(body, base_at);
    let series = c.uv64()?;
    let level_at = c.at();
    let level = c.u8()?;
    let width = crate::frame::width_of(levels, level).ok_or(FileError {
        at: level_at,
        refusal: Refusal::UnknownLevel,
    })?;
    let base_start = c.i64()?;
    let base = c.position()?;
    if c.done() {
        return fail(c.at(), Refusal::EmptyBlock);
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut prev_end: Option<i64> = None;
    while !c.done() {
        let at = c.at();
        let gap = c.uv64()?;
        let start = match prev_end {
            None if gap != 0 => return fail(at, Refusal::BaseStart),
            None => Some(base_start),
            Some(end) => i64::try_from(gap)
                .ok()
                .and_then(|g| g.checked_mul(MINUTE))
                .and_then(|d| end.checked_add(d)),
        };
        let start = start.ok_or(FileError {
            at,
            refusal: Refusal::Start,
        })?;
        let end = start.checked_add(width).ok_or(FileError {
            at,
            refusal: Refusal::Start,
        })?;
        if start.rem_euclid(width) != 0 {
            return fail(at, Refusal::Alignment);
        }
        let flags_at = c.at();
        let flags = c.u8()?;
        if flags & !1 != 0 {
            return fail(flags_at, Refusal::EntryFlag);
        }
        let as_of_at = c.at();
        let dg = c.uv64()?;
        let seq = c.uv64()?;
        let generation = u32::try_from(dg)
            .ok()
            .and_then(|d| base.generation.checked_add(d));
        let sequence = if dg == 0 {
            base.sequence.checked_add(seq)
        } else {
            Some(seq)
        };
        let (Some(generation), Some(sequence)) = (generation, sequence) else {
            return fail(as_of_at, Refusal::AsOf);
        };
        let n = c.uv64()?;
        let node_at = c.at();
        let raw = c.take(usize::try_from(n).unwrap_or(usize::MAX))?;
        let node = if raw.is_empty() {
            None
        } else {
            let node = crate::decode(raw).map_err(|e| node_error(e, node_at))?;
            if node.start != start || node.end != end {
                return fail(node_at, Refusal::NodeSpan);
            }
            if node.count == 0 {
                return fail(node_at, Refusal::NodeEmpty);
            }
            Some(node)
        };
        entries.push(Entry {
            start,
            stale: flags & 1 != 0,
            as_of: Position {
                generation,
                sequence,
            },
            node,
        });
        prev_end = Some(end);
    }
    Ok(Nodes {
        series,
        level,
        base,
        entries,
    })
}

fn node_error(e: DecodeError, node_at: u64) -> FileError {
    match e {
        DecodeError::UnsupportedForm(_) => FileError {
            at: node_at,
            refusal: Refusal::NodeForm,
        },
        DecodeError::Truncated(o)
        | DecodeError::Trailing(o)
        | DecodeError::Invalid { at: o, .. } => FileError {
            at: node_at + o as u64,
            refusal: Refusal::Node,
        },
    }
}
