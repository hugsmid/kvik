//! The attach checks (Q-162, Q-283): no other store's files, the key against the bucket's `STORE`, checksums
//! checked and reported, conditional create honoured. The reads come first; the probe object stays (the engine
//! never deletes).

use kvik_sink_api::{BackupSink, SinkError, Stored, Verified, VerifyError};

use super::Refused;
use super::driver::{Uploader, sha256};
use crate::envelope::{Envelope, EnvelopeError};
use crate::keys::Keys;
use crate::port::ManifestPort;

/// What the probe object holds, before sealing; the second differs, to test conditional create.
const PROBE: &[u8] = b"kvik attach probe";
const PROBE_OTHER: &[u8] = b"kvik attach probe, a second content";

impl<S: BackupSink, E: Envelope, M: ManifestPort> Uploader<S, E, M> {
    pub(super) async fn probe_bucket(&mut self) -> Result<(), Refused> {
        self.other_store().await?;
        self.store_key().await?;
        let key = self.keys.probe();
        let seal = |p: &[u8]| {
            self.envelope.seal(&key, p).map_err(|e| {
                Refused::Sink(SinkError::Failed {
                    reason: e.to_string(),
                })
            })
        };
        let (first, second) = (seal(PROBE)?, seal(PROBE_OTHER)?);
        let sha = sha256(&first);
        let mut wrong = sha;
        wrong[0] ^= 1;
        match self.sink.put(&key, first.clone().into(), wrong).await {
            Ok(_) => return Err(Refused::NoChecksums),
            Err(SinkError::HashMismatch { .. } | SinkError::Exists { .. }) => {}
            Err(e) => return Err(Refused::Sink(e)),
        }
        let expected = Stored::new(first.len() as u64, sha);
        if self.sink.put(&key, first.into(), sha).await? != expected {
            return Err(Refused::NoChecksums);
        }
        match Verified::check(&self.sink, &key, &expected).await {
            Ok(_) => {}
            Err(VerifyError::Sink(e)) => return Err(Refused::Sink(e)),
            Err(_) => return Err(Refused::NoChecksums),
        }
        let sha2 = sha256(&second);
        match self.sink.put(&key, second.into(), sha2).await {
            Ok(_) => Err(Refused::NoConditionalWrites),
            Err(SinkError::HashMismatch { .. } | SinkError::Exists { .. }) => Ok(()),
            Err(e) => Err(Refused::Sink(e)),
        }
    }

    /// Lists the bucket around this store's keys: any key under another store id refuses it (spec §8
    /// Generations). A listing that lags can miss a key; this check is best effort.
    async fn other_store(&mut self) -> Result<(), Refused> {
        let own = self.keys.store_id();
        let root = self.keys.store();
        let root = root.strip_suffix("STORE").unwrap_or(&root);
        // Above every key of this store and below every key of a store with a higher id.
        let past_own = format!("{}0", root.trim_end_matches('/'));
        let mut after: Option<String> = None;
        loop {
            let page = self.sink.list("", after.as_deref()).await?;
            let mut jump = false;
            for k in &page.keys {
                match Keys::store_id_of(k) {
                    Some(id) if id != own => return Err(Refused::OtherStore { store_id: id }),
                    Some(_) => {
                        jump = true;
                        break;
                    }
                    None => {}
                }
            }
            if jump && after.as_deref() < Some(past_own.as_str()) {
                after = Some(past_own.clone());
                continue;
            }
            match page.keys.last() {
                Some(last) if page.more => after = Some(last.clone()),
                _ => return Ok(()),
            }
        }
    }

    /// The bucket's `STORE`, if any, must open under this key.
    async fn store_key(&mut self) -> Result<(), Refused> {
        let key = self.keys.store();
        let header = match self
            .sink
            .get(&key, Some(0..self.envelope.header_len()))
            .await
        {
            Ok(h) => h,
            Err(SinkError::NotFound { .. }) => return Ok(()),
            Err(e) => return Err(Refused::Sink(e)),
        };
        match self.envelope.check_header(&header) {
            Ok(()) => Ok(()),
            Err(EnvelopeError::WrongKey) => Err(Refused::WrongKey),
            Err(e) => Err(Refused::Sink(SinkError::Failed {
                reason: format!("{key}: {e}"),
            })),
        }
    }
}
