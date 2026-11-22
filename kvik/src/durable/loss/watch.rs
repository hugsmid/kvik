//! The loss watch (Q-927): a log reader of the writer's process that moves only when `losses` or `health` asks,
//! reading everything written since the previous ask and the newest batch it had read, and keeping no facts.

use kvik_log::manifest::LostRange;
use kvik_log::reader::{LogReader, ReaderError};

use super::{Finder, Latch};
use crate::health::LossReport;

/// Items a catch-up takes from the log at a time.
pub(crate) const PART: usize = 1 << 16;

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct Watch {
    /// `None` after a shutdown.
    reader: Option<LogReader>,
    finder: Finder,
    /// The manifest's lost ranges as last read, kept when the reader is given up.
    recorded: Vec<LostRange>,
    part: usize,
}

impl Drop for Watch {
    fn drop(&mut self) {
        // Deletes the lease; a reader that cannot is a lease left to expire.
        if let Some(r) = self.reader.take() {
            let _ = r.close();
        }
    }
}

impl Watch {
    /// The watch of a reader that has read the log so far, every event of it through `finder`.
    pub(crate) fn from_start(reader: LogReader, finder: Finder) -> Self {
        Watch::with_part(reader, finder, PART)
    }

    /// A watch that takes `part` items at a time.
    pub(crate) fn with_part(reader: LogReader, finder: Finder, part: usize) -> Self {
        Watch {
            recorded: reader.lost_ranges().to_vec(),
            reader: Some(reader),
            finder,
            part,
        }
    }

    /// Reads what was written since the last call, one part at a time, noting the losses found in `latch`. Keeps no
    /// segment bytes afterwards, failed or not. After a shutdown, reads nothing.
    pub(crate) fn catch_up(&mut self, latch: &Latch) -> Result<(), ReaderError> {
        let Some(reader) = self.reader.as_mut() else {
            return Ok(());
        };
        let r = read(
            reader,
            &mut self.finder,
            &mut self.recorded,
            self.part,
            latch,
        );
        reader.release_bytes();
        r
    }

    /// Gives up the reader, deleting its lease; the reports stay as last read.
    pub(crate) fn close(&mut self) {
        if let Some(r) = self.reader.take() {
            let _ = r.close();
        }
    }

    /// The files' loss reports as of the last catch-up, with the losses this watch found.
    pub(crate) fn reports(&self) -> Vec<LossReport> {
        self.finder.losses(&self.recorded)
    }

    /// The manifest's lost ranges as last read.
    pub(crate) fn recorded(&self) -> &[LostRange] {
        &self.recorded
    }

    /// Bytes its reader keeps between catch-ups.
    #[cfg(test)]
    pub(crate) fn kept_bytes(&self) -> usize {
        self.reader.as_ref().map_or(0, LogReader::kept_bytes)
    }
}

fn read(
    reader: &mut LogReader,
    finder: &mut Finder,
    recorded: &mut Vec<LostRange>,
    part: usize,
    latch: &Latch,
) -> Result<(), ReaderError> {
    loop {
        let (u, more) = reader.refresh_some(part)?;
        recorded.clear();
        recorded.extend_from_slice(reader.lost_ranges());
        let found = finder.apply(&u.events, u.state.durable);
        if !found.is_empty() {
            latch.note(&found);
        }
        if !more {
            return Ok(());
        }
    }
}
