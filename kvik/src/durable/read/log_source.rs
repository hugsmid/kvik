//! The log tail as a source (the query design's §2.2): every position above C(E) up to the cut, read through the
//! key-history lookup of roadmap 11.0 (`format.md` A.8 indexes and a scan of segments without one) at the view's
//! cut. Compaction does not exist yet, so C(E) is the origin and the tail is the whole kept log (Q-1060).

use kvik_log::reader::{Wanted, Within};

use super::merge;
use super::source::{Pass, Read, Source};
use super::view::View;
use crate::durable::index::Stored;
use crate::durable::reader::DurableReader;
use crate::error::QueryError;

// The facts' vector is reused for the rows (`cache::held` counts its capacity).
const _: () = assert!(
    std::mem::size_of::<Stored>() == std::mem::size_of::<kvik_log::reader::FoundFact>()
        && std::mem::align_of::<Stored>() == std::mem::align_of::<kvik_log::reader::FoundFact>()
);

/// See the module documentation; `listed` holds the segments its last read listed.
#[derive(Debug)]
pub(crate) struct LogTail<'a> {
    pub(crate) reader: &'a DurableReader,
    pub(crate) view: &'a View,
    pub(crate) listed: Vec<u64>,
}

/// The lookup ranges of `pass`.
pub(crate) fn wanted(pass: &Pass<'_>) -> Vec<Wanted> {
    pass.series
        .iter()
        .map(|&series_id| Wanted {
            series_id,
            from: pass.from,
            to: pass.to,
        })
        .collect()
}

impl Source for LogTail<'_> {
    fn read(&mut self, pass: &Pass<'_>, budget: u64, expect: u64) -> Result<Read, QueryError> {
        let wanted = wanted(pass);
        let within = Within {
            budget,
            above: pass.above,
            upto: Some(pass.upto),
            expect,
        };
        let (found, listed) = self.reader.lookup_within(self.view, &wanted, within)?;
        self.listed = listed;
        let Some(found) = found else {
            return Ok(Read::TooMany);
        };
        let mut rows: Vec<Stored> = found
            .into_iter()
            .map(|f| Stored {
                position: f.position,
                received_at: f.received_at,
                row: f.row,
            })
            .collect();
        let room = budget.saturating_sub(super::cache::held(&rows));
        merge::sort(&mut rows, room);
        Ok(Read::Rows(rows))
    }
}
