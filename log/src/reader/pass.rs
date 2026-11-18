//! One pass over the log (`spec.md` §7 Readers): the check of the last batches returned, then the segments from the
//! one followed to the mark's, below the mark only.

use super::files::read_part;
use super::{
    Fingerprint, LogReader, MAX_SEGMENT_FILE, RECENT, ReadBatch, ReadEvent, ReaderError, Scratch,
    View, VoidCause, fingerprint, io_err,
};
use crate::batch::SegmentIdentity;
use crate::content::BatchContent;
use crate::header::HEADER_LEN;
use crate::mark::MarkSlot;
use crate::name::SegmentName;
use crate::scan::{Found, Resume, ScanLimit, Scanner, set_aside};
use crate::writer::Position;

impl LogReader {
    pub(super) fn read_log(
        &self,
        view: &mut View,
        mark: &MarkSlot,
        events: &mut Vec<ReadEvent>,
        scratch: &mut Scratch,
        budget: Option<usize>,
    ) -> Result<bool, ReaderError> {
        let mut items = 0usize;
        // A new generation: everything taken is void and the log is read again from its start (Q-118 (c)).
        match view.generation {
            Some(g) if g != mark.generation => {
                if view.taken != Position::default() {
                    events.push(ReadEvent::Void {
                        above: Position::default(),
                        cause: VoidCause::Generation {
                            from: g,
                            to: mark.generation,
                        },
                    });
                }
                *view = View::default();
            }
            _ => {}
        }
        view.generation = Some(mark.generation);
        self.check_recent(view, events)?;
        let mut listing = None;
        let (first, first_generation) = (view.file_id, view.segment_generation);
        let names: Box<dyn Iterator<Item = SegmentName>> = if first == 0 {
            Box::new(self.listed(&mut listing, first, mark)?.to_vec().into_iter())
        } else {
            // Caught up: the segments from the one followed to the mark's, by file id (Q-852).
            Box::new((first..=mark.file_id).map(move |file_id| SegmentName {
                generation: if file_id == first {
                    first_generation
                } else {
                    mark.generation
                },
                file_id,
                sealed: file_id < mark.file_id,
            }))
        };
        for name in names {
            let file_id = name.file_id;
            let limit = if file_id == mark.file_id {
                ScanLimit::Mark(mark.offset)
            } else {
                ScanLimit::End
            };
            let resumed = if file_id == view.file_id {
                view.resume
            } else {
                None
            };
            let from = resumed.unwrap_or(Resume {
                offset: HEADER_LEN as u64,
                last_sequence: None,
            });
            if let ScanLimit::Mark(m) = limit {
                let m = m.max(HEADER_LEN as u64);
                // The mark is behind the point reached in its segment, or at it: nothing new is below it. At it, the
                // pass that reached that point checked the header.
                if from.offset > m || (resumed.is_some() && from.offset == m) {
                    break;
                }
            }
            // Not found by name: what a listing holds decides, as on a first pass. An earlier segment listed but gone
            // would leave its positions unread: an error, as for the mark's.
            let opened = match self.open_segment(name)? {
                Some(o) => Some(o),
                // The mark's segment is listed, or `listed` failed.
                None => match self
                    .listed(&mut listing, first, mark)?
                    .iter()
                    .find(|n| n.file_id == file_id)
                {
                    Some(&n) => self.open_segment(n)?,
                    None => continue,
                },
            };
            let Some((n, f)) = opened else {
                return Err(ReaderError::SegmentMissing { file_id });
            };
            let path = self.log_dir.join(n.to_string());
            let len = match limit {
                ScanLimit::Mark(m) => m,
                ScanLimit::End => f.len().map_err(io_err(&path))?,
            };
            if len > MAX_SEGMENT_FILE {
                return Err(ReaderError::TooLong { path, len });
            }
            let whole = matches!(limit, ScanLimit::End);
            let follows = (!whole).then_some(file_id);
            let kept = whole && resumed.is_some() && scratch.whole == Some(file_id);
            scratch.whole = None;
            if !kept {
                if scratch.file_id != follows {
                    scratch.bytes = Vec::new();
                }
                scratch.file_id = follows;
                read_part(&mut scratch.bytes, &*f, from.offset, len, whole)
                    .map_err(io_err(&path))?;
            }
            let bytes = &scratch.bytes[..];
            let id = SegmentIdentity {
                store_id: self.store_id,
                file_id,
                generation: n.generation,
            };
            let refused = |error| ReaderError::Unsupported {
                path: path.clone(),
                error,
            };
            let mut s = Scanner::resume(bytes, &id, limit, from)
                .map_err(refused)?
                .keeping_content();
            view.file_id = file_id;
            view.segment_generation = n.generation;
            view.resume = Some(from);
            loop {
                let before = s.resume_point();
                let Some(found) = s.next() else { break };
                match found.map_err(refused)? {
                    Found::Region(region) => {
                        if view.regions.insert((file_id, region.start, region.end)) {
                            events.push(ReadEvent::Corruption { file_id, region });
                        }
                    }
                    Found::Batch(b) if set_aside(&b, &self.cfg.set_aside) => {}
                    Found::Batch(b) => {
                        let g = b.commit.generation;
                        let last = Position {
                            generation: g,
                            sequence: b.commit.last_sequence,
                        };
                        // Never above the mark's last position, from any segment.
                        if g == mark.generation && last.sequence > mark.last_sequence {
                            view.resume = Some(before);
                            fingerprint(&mut view.recent, bytes);
                            return Ok(false);
                        }
                        let content = match s.take_content() {
                            Some(c) => c,
                            None => {
                                BatchContent::decode(&b).map_err(|error| ReaderError::Content {
                                    path: path.clone(),
                                    offset: b.offset,
                                    error,
                                })?
                            }
                        };
                        let rb = ReadBatch {
                            file_id,
                            offset: b.offset,
                            first: Position {
                                generation: g,
                                sequence: b.commit.first_sequence,
                            },
                            last,
                            commit_time: b.commit.commit_time,
                            content,
                        };
                        // Reading only moves forward past batches not read before, or past the last batch that
                        // still reads the same, so every entry met is applied: one written again at the offset
                        // of an earlier one (adopted, Q-117) still removed what followed.
                        if let Some(r) = rb.recovery() {
                            let after = Position {
                                generation: g,
                                sequence: r.after,
                            };
                            if after < view.taken {
                                events.push(ReadEvent::Void {
                                    above: after,
                                    cause: VoidCause::Recovery { entry: rb.first },
                                });
                                view.taken = after;
                                view.recent.retain(|f| f.last <= after);
                            }
                        }
                        if last > view.taken {
                            view.taken = last;
                            view.recent.push_back(Fingerprint {
                                file_id,
                                segment: n,
                                offset: b.offset,
                                end: b.end(),
                                crc: None,
                                first: rb.first,
                                last,
                            });
                            if view.recent.len() > RECENT {
                                view.recent.pop_front();
                            }
                            items += rb.content.items.len();
                            events.push(ReadEvent::Batch(rb));
                        }
                    }
                }
                view.resume = Some(s.resume_point());
                if budget.is_some_and(|b| items >= b) {
                    fingerprint(&mut view.recent, bytes);
                    if whole {
                        scratch.whole = Some(file_id);
                    }
                    return Ok(true);
                }
            }
            fingerprint(&mut view.recent, bytes);
        }
        Ok(false)
    }

    /// Checks that the last batch returned still reads the same. If not, voids what was taken above the last
    /// remembered batch that does, and reads again from there; with none left, from the start of the oldest
    /// remembered batch's segment, where every recovery entry is applied again: the one the truncating start wrote
    /// voids the rest, and an earlier one may void batches that are intact, which are then returned again
    /// (`spec.md` §3 Recovery).
    fn check_recent(
        &self,
        view: &mut View,
        events: &mut Vec<ReadEvent>,
    ) -> Result<(), ReaderError> {
        let Some(last) = view.recent.back() else {
            return Ok(());
        };
        if self.still_reads(last)? {
            return Ok(());
        }
        let mut keep = 0;
        for fp in &view.recent {
            if !self.still_reads(fp)? {
                break;
            }
            keep += 1;
        }
        let oldest = view.recent[0];
        let above = match keep {
            0 => Position {
                generation: oldest.first.generation,
                sequence: oldest.first.sequence.saturating_sub(1),
            },
            k => view.recent[k - 1].last,
        };
        if above < view.taken {
            events.push(ReadEvent::Void {
                above,
                cause: VoidCause::Rewritten,
            });
            view.taken = above;
        }
        view.recent.truncate(keep);
        match view.recent.back() {
            Some(fp) => {
                view.file_id = fp.file_id;
                view.segment_generation = fp.segment.generation;
                view.resume = Some(Resume {
                    offset: fp.end,
                    last_sequence: Some(fp.last.sequence),
                });
            }
            None => {
                view.file_id = oldest.file_id;
                view.segment_generation = oldest.segment.generation;
                view.resume = None;
            }
        }
        Ok(())
    }

    /// The segments of a listing from file id `first` to the mark's, the mark's required; the listing is taken once
    /// per pass into `listing`.
    fn listed<'a>(
        &self,
        listing: &'a mut Option<Vec<SegmentName>>,
        first: u64,
        mark: &MarkSlot,
    ) -> Result<&'a [SegmentName], ReaderError> {
        if let Some(l) = listing {
            return Ok(l);
        }
        let mut names: Vec<SegmentName> = self
            .segments()?
            .into_iter()
            // Segments of earlier generations are sealed and read whole, whichever segment the mark names (Q-268).
            .filter(|n| {
                n.file_id > 0
                    && n.file_id >= first
                    && (n.file_id <= mark.file_id || n.generation < mark.generation)
            })
            .collect();
        if mark.file_id > 0
            && mark.file_id >= first
            && names.last().is_none_or(|n| n.file_id != mark.file_id)
        {
            // Renamed between the listing and now, or gone.
            match self.find_segment(mark.file_id)? {
                Some(n) => names.push(n),
                None => {
                    return Err(ReaderError::SegmentMissing {
                        file_id: mark.file_id,
                    });
                }
            }
        }
        Ok(listing.insert(names))
    }
}
