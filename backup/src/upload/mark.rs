//! The upload driver's last-uploaded markers (`format.md` A.17; Q-614): read at the first step, put after each record.

use std::io;

use kvik_sink_api::BackupSink;

use super::driver::{Out, Stop, Uploader, io_stop, put_stop, sink_stop};
use crate::envelope::Envelope;
use crate::marker::{self, PutError, ReadError, Summary};
use crate::port::ManifestPort;
use crate::uploaded::UploadedRecord;

impl<S: BackupSink, E: Envelope, M: ManifestPort> Uploader<S, E, M> {
    /// Whether a record went up that no marker names yet.
    pub(super) fn marker_due(&self) -> bool {
        self.cursor
            .as_ref()
            .is_some_and(|c| c.summary.epoch > c.marked.epoch)
    }

    /// What the latest marker of `g` names, the summary of the records `listed` (those above it fetched and applied)
    /// and the sequence the next marker tries. A latest marker that cannot be read counts as none, so the next marker,
    /// made from every record listed, replaces it; so does a bucket written before markers.
    pub(super) async fn open_marks(
        &mut self,
        g: u32,
        mut listed: Vec<u64>,
    ) -> Out<(Summary, Summary, u64)> {
        let latest = marker::latest(&self.sink, &self.keys, g)
            .await
            .map_err(sink_stop)?;
        let mut marked = Summary::default();
        if latest > 0 {
            match marker::read(&self.sink, &self.envelope, &self.keys, g, latest).await {
                Ok(m) => marked = m.summary,
                Err(ReadError::Unreadable(_)) => {}
                Err(ReadError::Sink(e)) => return Err(sink_stop(e)),
            }
        }
        listed.sort_unstable();
        listed.dedup();
        let mut summary = marked;
        for epoch in listed.into_iter().filter(|e| *e > marked.epoch) {
            let key = self.keys.record(g, epoch);
            let bytes = self.sink.get(&key, None).await.map_err(sink_stop)?;
            let plain = self
                .envelope
                .open(&key, &bytes)
                .map_err(|e| Stop::Fail(e.into()))?;
            let u = UploadedRecord::decode(&plain)
                .map_err(|e| io_stop(io::Error::new(io::ErrorKind::InvalidData, e)))?;
            summary.apply(&u.record);
        }
        Ok((marked, summary, latest.saturating_add(1)))
    }

    /// Puts the marker that is due, at the next free sequence (Q-614).
    pub(super) async fn put_marker(&mut self, g: u32) -> Out<()> {
        let Some((from, summary)) = self.cursor.as_ref().map(|c| (c.next_marker, c.summary)) else {
            return Ok(());
        };
        let sequence = marker::put(&self.sink, &self.envelope, &self.keys, g, from, summary)
            .await
            .map_err(|e| match e {
                PutError::Sink(e) => put_stop(e),
                PutError::Envelope(e) => Stop::Fail(e.into()),
            })?;
        if let Some(c) = self.cursor.as_mut() {
            c.marked = summary;
            c.next_marker = sequence.saturating_add(1);
        }
        Ok(())
    }
}
