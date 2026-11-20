//! The log tail as a source (the query design's §2.2): every position above C(E) up to the cut, read through the
//! key-history lookup of roadmap 11.0 (`format.md` A.8 indexes and a scan of segments without one) at the view's
//! cut. Compaction does not exist yet, so C(E) is the origin and the tail is the whole kept log (Q-1060).

use kvik_log::reader::Wanted;

use super::merge;
use super::source::{Pass, Read, Source};
use super::view::View;
use crate::durable::index::Stored;
use crate::durable::reader::DurableReader;
use crate::error::QueryError;

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct LogTail<'a> {
    pub(crate) reader: &'a DurableReader,
    pub(crate) view: &'a View,
}

impl Source for LogTail<'_> {
    fn read(&mut self, pass: &Pass<'_>, budget: u64) -> Result<Read, QueryError> {
        let wanted: Vec<Wanted> = pass
            .series
            .iter()
            .map(|&series_id| Wanted {
                series_id,
                from: pass.from,
                to: pass.to,
            })
            .collect();
        let Some(found) = self.reader.lookup_within(self.view, &wanted, budget)? else {
            return Ok(Read::TooMany);
        };
        let mut rows: Vec<Stored> = found
            .into_iter()
            .filter(|f| f.position <= pass.bound)
            .map(|f| Stored {
                position: f.position,
                received_at: f.received_at,
                row: f.row,
            })
            .collect();
        merge::sort(&mut rows);
        Ok(Read::Rows(rows))
    }
}
