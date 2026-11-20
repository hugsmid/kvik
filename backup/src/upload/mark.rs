//! The upload driver's last-uploaded markers (`format.md` A.17; Q-614): read at the first step, put after each record.

use std::io;

use kvik_sink_api::{BackupSink, SinkError};

use super::UploadError;
use super::driver::{Out, Stop, Uploader, io_stop, put_stop, sink_stop};
use crate::envelope::Envelope;
use crate::marker::{self, PutError, ReadError, Summary};
use crate::plan::after_last;
use crate::port::ManifestPort;
use crate::uploaded::{UploadedRecord, opens_manifest};

impl<S: BackupSink, E: Envelope, M: ManifestPort> Uploader<S, E, M> {
    /// Whether a record went up that no marker names yet.
    pub(super) fn marker_due(&self) -> bool {
        self.cursor
            .as_ref()
            .is_some_and(|c| c.summary.epoch > c.marked.epoch)
    }

    /// What the latest marker of `g` names, the summary of the records in the bucket (those above it fetched and
    /// applied) and the sequence the next marker tries. The records above it are those `listed` and those `stat`
    /// reports within `TAIL_PROBES` epochs after the highest known, as a restore asks (Q-612): a record put just before
    /// a restart that the listing hides is still counted. Their uploaded forms must chain from the marker's epoch; a
    /// break whose predecessor `stat` reports is a lagging listing, retried (Q-614). The marker counted is the latest
    /// readable one, as at a restore; with none readable, or none at all (a bucket written before markers), the next
    /// marker is made from every record found.
    pub(super) async fn open_marks(
        &mut self,
        g: u32,
        mut listed: Vec<u64>,
    ) -> Out<(Summary, Summary, u64)> {
        let latest = marker::latest(&self.sink, &self.keys, g)
            .await
            .map_err(sink_stop)?;
        let marked = self.latest_readable(g, latest).await?;
        listed.sort_unstable();
        listed.dedup();
        let mut found = Vec::new();
        for epoch in listed.into_iter().filter(|e| *e > marked.epoch) {
            found.push(self.fetch_record(g, epoch).await?);
        }
        let mut top = found.last().map_or(marked.epoch, |u| u.record.epoch);
        'probe: loop {
            for epoch in after_last(top) {
                if self.record_reported(g, epoch).await? {
                    found.push(self.fetch_record(g, epoch).await?);
                    top = epoch;
                    continue 'probe;
                }
            }
            break;
        }
        let mut before = marked.epoch;
        for u in &found {
            let want = if opens_manifest(&u.record) { 0 } else { before };
            if u.previous != want
                && u.previous > before
                && self.record_reported(g, u.previous).await?
            {
                return Err(Stop::Retry(SinkError::Transient {
                    reason: format!(
                        "the listing of generation {g}'s records does not show record {}, which record {} names",
                        u.previous, u.record.epoch
                    ),
                }));
            }
            before = u.record.epoch;
        }
        let mut summary = marked;
        for u in &found {
            summary.apply(&u.record);
        }
        Ok((marked, summary, latest.saturating_add(1)))
    }

    /// What the latest readable marker of `g` at or below `latest` names, scanning down past those that do not open,
    /// decode or match their key, or that the bucket no longer holds (Q-614 (c)); one of a version this build does not
    /// read stops uploads (Q-960). The scan has no bound (Q-1030).
    async fn latest_readable(&mut self, g: u32, latest: u64) -> Out<Summary> {
        for sequence in (1..=latest).rev() {
            match marker::read(&self.sink, &self.envelope, &self.keys, g, sequence).await {
                Ok(m) => return Ok(m.summary),
                Err(ReadError::Unreadable(_) | ReadError::Sink(SinkError::NotFound { .. })) => {}
                Err(ReadError::Version(version)) => {
                    return Err(Stop::Fail(UploadError::MarkerVersion { sequence, version }));
                }
                Err(ReadError::Sink(e)) => return Err(sink_stop(e)),
            }
        }
        Ok(Summary::default())
    }

    async fn record_reported(&mut self, g: u32, epoch: u64) -> Out<bool> {
        Ok(self
            .sink
            .stat(&self.keys.record(g, epoch))
            .await
            .map_err(sink_stop)?
            .is_some())
    }

    async fn fetch_record(&mut self, g: u32, epoch: u64) -> Out<UploadedRecord> {
        let key = self.keys.record(g, epoch);
        let bytes = self.sink.get(&key, None).await.map_err(sink_stop)?;
        let plain = self
            .envelope
            .open(&key, &bytes)
            .map_err(|e| Stop::Fail(e.into()))?;
        UploadedRecord::decode(&plain)
            .map_err(|e| io_stop(io::Error::new(io::ErrorKind::InvalidData, e)))
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
