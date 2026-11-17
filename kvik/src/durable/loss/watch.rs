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
    reader: Option<LogReader>,
    finder: Finder,
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
    pub(crate) fn new(reader: LogReader) -> Self {
        Watch::with_part(reader, PART)
    }

    /// A watch that takes `part` items at a time.
    pub(crate) fn with_part(reader: LogReader, part: usize) -> Self {
        Watch {
            reader: Some(reader),
            finder: Finder::default(),
            part,
        }
    }

    fn reader(&mut self) -> &mut LogReader {
        self.reader.as_mut().expect("taken only by drop")
    }

    /// Reads what was written since the last call, one part at a time, noting the losses found in `latch`. Keeps no
    /// segment bytes afterwards, failed or not.
    pub(crate) fn catch_up(&mut self, latch: &Latch) -> Result<(), ReaderError> {
        let r = self.read(latch);
        self.reader().release_bytes();
        r
    }

    fn read(&mut self, latch: &Latch) -> Result<(), ReaderError> {
        loop {
            let part = self.part;
            let (u, more) = self.reader().refresh_some(part)?;
            let found = self.finder.apply(&u.events, u.state.durable);
            if !found.is_empty() {
                latch.note(&found);
            }
            if !more {
                return Ok(());
            }
        }
    }

    /// The files' loss reports as of the last catch-up, with the losses this watch found.
    pub(crate) fn reports(&self) -> Vec<LossReport> {
        let recorded: &[LostRange] = self.reader.as_ref().map_or(&[], |r| r.lost_ranges());
        self.finder.losses(recorded)
    }

    /// Bytes its reader keeps between catch-ups.
    #[cfg(test)]
    pub(crate) fn kept_bytes(&self) -> usize {
        self.reader.as_ref().map_or(0, LogReader::kept_bytes)
    }
}
