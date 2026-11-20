//! A lookup's scan of what follows a segment's map: complete batches up to the mark read into buffers of their own
//! length, anything else scanned a part at a time.

use super::*;

/// Scans what follows the map's resume point up to the limit, a part at a time, adding batches and regions to the
/// map and the wanted facts to `facts`; `None` if the segment could not be read. A part ends at [`PIECE`] bytes past
/// where it starts; a region running to a part's end that may be a batch cut by it is read again in a longer part.
#[allow(clippy::too_many_arguments)]
pub(super) fn scan_new(
    f: &dyn crate::fs::FsFile,
    id: &SegmentIdentity,
    len: u64,
    limit: ScanLimit,
    part_bytes: u64,
    query: &Query<'_>,
    map: &mut SegmentMap,
    first: usize,
    facts: &mut Vec<FoundFact>,
) -> Option<()> {
    let mut from = map.resume.unwrap_or(Resume {
        offset: HEADER_LEN as u64,
        last_sequence: None,
    });
    let mut piece = part_bytes;
    // Every part's scan reads the header first and refuses the segment on it; complete batches up to the mark are
    // then read into a buffer of their own length, and only what is not one is scanned in a buffer from the file's
    // start.
    let header = header(f, id)?;
    loop {
        let to = from
            .offset
            .saturating_add(piece)
            .next_multiple_of(8)
            .min(len);
        let last = to >= len;
        if !last && from.offset >= to {
            return Some(());
        }
        let lim = if last { limit } else { ScanLimit::Mark(to) };
        if let ScanLimit::Mark(m) = lim
            && from.offset <= m
        {
            match take_complete(f, id, header, from, m, query, map, first, facts)? {
                Taken::Stopped => return Some(()),
                Taken::To(p) if p.offset == m => {
                    from = p;
                    if last {
                        map.resume = Some(from);
                        return Some(());
                    }
                    piece = part_bytes;
                    continue;
                }
                Taken::To(p) => from = p,
            }
        }
        let to = from
            .offset
            .saturating_add(piece)
            .next_multiple_of(8)
            .min(len)
            .max(to);
        let last = to >= len;
        let lim = if last { limit } else { ScanLimit::Mark(to) };
        let buf = part(
            f,
            if last { len } else { to },
            from.offset,
            if last { len } else { to },
        )?;
        let mut scanner = Scanner::resume(&buf, id, lim, from).ok()?.keeping_content();
        // Each finding is taken as it is read; only a region is held back, since one running to the end of a part
        // that is not the last may be a batch the part cut.
        let mut held: Option<(InvalidRegion, Resume)> = None;
        let mut found = 0usize;
        loop {
            let before = scanner.resume_point();
            let x = match scanner.next() {
                None => break,
                Some(Err(_)) => return None,
                Some(Ok(x)) => x,
            };
            let after = scanner.resume_point();
            found += 1;
            if let Some((r, at)) = held.take() {
                map.regions.push((r.start, r.end));
                map.resume = Some(at);
            }
            match x {
                Found::Region(r) => held = Some((r, after)),
                Found::Batch(b) => {
                    let kept = scanner.take_content();
                    if take_found(&b, kept, before, after, query, map, first, facts) {
                        return Some(());
                    }
                }
            }
        }
        let end = scanner.resume_point();
        let cut = !last && held.as_ref().is_some_and(|(r, _)| r.end == to);
        if cut && found == 1 {
            piece = piece.saturating_mul(2);
            continue;
        }
        if let Some((r, at)) = held.filter(|_| !cut) {
            map.regions.push((r.start, r.end));
            map.resume = Some(at);
        }
        if last && !cut {
            map.resume = Some(end);
            return Some(());
        }
        from = map.resume.unwrap_or(from);
        piece = part_bytes;
    }
}

/// How far [`take_complete`] took the batches it read.
enum Taken {
    /// Up to this point: the limit, or the start of what is not a complete batch.
    To(Resume),
    /// At a batch above the mark's last position.
    Stopped,
}

/// Takes the complete batches from `from` up to `limit` as a scan to the mark would, each read into a buffer of its own
/// length; `None` on a refusal.
#[allow(clippy::too_many_arguments)]
fn take_complete(
    f: &dyn crate::fs::FsFile,
    id: &SegmentIdentity,
    header: HeaderState,
    mut from: Resume,
    limit: u64,
    query: &Query<'_>,
    map: &mut SegmentMap,
    first: usize,
    facts: &mut Vec<FoundFact>,
) -> Option<Taken> {
    let mut buf = buffer(usize::try_from(limit - from.offset).ok()?);
    let got = read_at_full(f, &mut buf, from.offset).ok()?;
    buf[got..].fill(0);
    let base = from.offset;
    let header_first = match header {
        HeaderState::Valid(h) => h.base_sequence,
        HeaderState::Invalid => 0,
    };
    while from.offset < limit {
        let min_first = from
            .last_sequence
            .map_or(header_first, |l| l.saturating_add(1));
        let (b, bodies) = match read_batch_in(
            &buf,
            base,
            id,
            from.offset,
            limit,
            min_first,
            Refusal::AtOnce,
            true,
        ) {
            BatchRead::Complete(b, bodies) => (b, bodies),
            BatchRead::Refused(_) => return None,
            BatchRead::Incomplete { .. } => break,
        };
        let after = Resume {
            offset: b.end(),
            last_sequence: Some(b.commit.last_sequence),
        };
        let content = BatchContent::from_bodies(b.commit.first_sequence, bodies).ok();
        if take_found(&b, content, from, after, query, map, first, facts) {
            return Some(Taken::Stopped);
        }
        from = after;
    }
    Some(Taken::To(from))
}

/// Drops the facts this segment's read took, from index `first` of `facts`, above a recovery entry's `after`.
pub(super) fn void_above(facts: &mut Vec<FoundFact>, first: usize, after: Position) {
    let mut j = 0;
    facts.retain(|x| {
        j += 1;
        j <= first || x.position <= after
    });
}

/// Takes a complete batch a scan found into the map and the wanted facts; returns whether the scan stops at it, being
/// above the mark's last position.
#[allow(clippy::too_many_arguments)]
fn take_found(
    b: &crate::batch::Batch<'_>,
    kept: Option<BatchContent>,
    before: Resume,
    after: Resume,
    query: &Query<'_>,
    map: &mut SegmentMap,
    first: usize,
    facts: &mut Vec<FoundFact>,
) -> bool {
    let g = b.commit.generation;
    if g == query.generation && b.commit.last_sequence > query.last_sequence {
        map.resume = Some(before);
        return true;
    }
    let Some(content) = kept.or_else(|| BatchContent::decode(b).ok()) else {
        // A guard: the scanner ran the same body checks, so a complete batch decodes. Kept as a region, so that every
        // later lookup of the segment is incomplete too.
        map.regions.push((b.offset, b.end()));
        map.resume = Some(after);
        return false;
    };
    let (min_t, max_t) = content
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Fact(r) => Some(r.t),
            _ => None,
        })
        .fold((i64::MAX, i64::MIN), |(a, z), t| (a.min(t), z.max(t)));
    map.batches.push(Span {
        offset: b.offset,
        end: b.end(),
        generation: g,
        first: b.commit.first_sequence,
        last: b.commit.last_sequence,
        min_t,
        max_t,
        commit_time: b.commit.commit_time,
    });
    if let Some(r) = content.recovery() {
        let after = Position {
            generation: g,
            sequence: r.after,
        };
        map.recoveries.push((map.batches.len() - 1, after));
        void_above(facts, first, after);
    }
    query.take(content, g, b.commit.first_sequence, facts);
    map.resume = Some(after);
    false
}
