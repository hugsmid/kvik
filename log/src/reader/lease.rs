//! A reader's lease and cuts (`spec.md` §7 Readers, query design §3.3): the lease names the lowest epoch of the live
//! cuts and records the highest epoch read; it is written when either would change or half of `lease_refresh` has
//! passed; a cut lives `cut_max_age` from the last manifest check that found its epoch current; a lease found gone or
//! changed ends every cut.

use std::io;

use super::files::read_upto;
use super::{Cut, LogReader, ReaderError, io_err};
use crate::fs::OpenMode;
use crate::header::{FileHeader, HEADER_LEN, kind};
use crate::lease::{LEASE_EXPIRY, LEASE_LEN, LeaseSlot, SLOT_OFFSET};

/// What a renewal wrote, and whether it found the lease gone or changed.
#[derive(Clone, Copy, Debug)]
pub(super) struct Renewal {
    slot: LeaseSlot,
    image: [u8; LEASE_LEN],
    lost: bool,
}

impl LogReader {
    /// Takes the manifest as the files now hold it into what the reader holds of it. The live cuts of the epoch it
    /// finds current count their age from now.
    pub(super) fn take_manifest(&mut self) -> Result<(), ReaderError> {
        if let Some(m) = self.check_manifest()? {
            self.manifest = m;
            self.lost_stale = true;
        }
        let now = self.cfg.clock.monotonic();
        self.checked_at = now;
        self.cuts
            .seen(self.manifest.epoch, now, self.cfg.cut_max_age);
        Ok(())
    }

    /// Whether a pass at `epoch` writes the lease: never written, the epoch it names or the highest epoch read would
    /// change, or half of `lease_refresh` has passed.
    pub(super) fn renewal_due(&self, epoch: u64) -> bool {
        self.image.is_none()
            || self.lease_epoch(epoch) != self.lease.epoch
            || self.manifest.epoch > self.lease.highest_read
            || self.cfg.clock.monotonic().saturating_sub(self.renewed_at)
                >= self.cfg.lease_refresh / 2
    }

    /// Renews the lease if half of `lease_refresh` has passed since it was written, naming the lowest epoch of the
    /// live cuts and of the lease, or, once the lease may have expired, of the manifest's current epoch. Returns
    /// whether it wrote the lease. A read call that does not refresh calls this (`api.md` Reader processes).
    pub fn keep_alive(&mut self) -> Result<bool, ReaderError> {
        let elapsed = self.cfg.clock.monotonic().saturating_sub(self.renewed_at);
        if elapsed < self.cfg.lease_refresh / 2 {
            return Ok(false);
        }
        let epoch = if elapsed >= LEASE_EXPIRY {
            self.take_manifest()?;
            self.manifest.epoch
        } else {
            self.lease.epoch
        };
        self.renew(epoch, self.state.generation)?;
        Ok(true)
    }

    /// Takes a cut at what the reader holds (query design §3.2): the manifest epoch it last read, the durable position
    /// and the reset stamp. The lease names that epoch or a lower one while the cut is live. Its age counts from the
    /// last manifest check that found its epoch current, not from now.
    pub fn hold(&mut self) -> Cut {
        let epoch = self.manifest.epoch;
        Cut {
            id: self.cuts.take(epoch, self.checked_at),
            epoch,
            durable: self.state.durable,
            stamp: self.state.stamp,
        }
    }

    /// Whether the cut `id` is live, after renewing the lease if due ([`LogReader::keep_alive`]): held, its epoch
    /// found current by a manifest check less than `cut_max_age` ago, and not ended by a lease found gone or changed.
    /// An ended cut is [`ReaderError::CutEnded`].
    pub fn check_cut(&mut self, id: u64) -> Result<(), ReaderError> {
        self.keep_alive()?;
        self.cuts
            .check(id, self.cfg.clock.monotonic(), self.cfg.cut_max_age)
            .map(|_| ())
            .map_err(ReaderError::CutEnded)
    }

    /// Ends the cut `id`; the lease may name a later epoch from the next renewal.
    pub fn release(&mut self, id: u64) {
        self.cuts.release(id);
    }

    /// How many cuts are held and not released.
    pub fn cuts_held(&self) -> usize {
        self.cuts.len()
    }

    /// The epoch the lease would name now: the lowest of the live cuts and `epoch`, the epoch of the pass about to
    /// read.
    pub(super) fn lease_epoch(&self, epoch: u64) -> u64 {
        let now = self.cfg.clock.monotonic();
        self.cuts
            .lowest(now, self.cfg.cut_max_age)
            .map_or(epoch, |l| l.min(epoch))
    }

    pub(super) fn renew(&mut self, epoch: u64, generation: u32) -> Result<(), ReaderError> {
        let s = self.renew_slot(epoch, generation)?;
        self.commit_lease(s);
        Ok(())
    }

    /// Writes the lease with the next counter, by path, naming the lowest epoch of the live cuts and `epoch`; creates
    /// it again if the writer deleted it. A lease found gone or changed since the reader last wrote it ends every
    /// cut, so it then names `epoch` (`spec.md` §7 Readers).
    pub(super) fn renew_slot(&self, epoch: u64, generation: u32) -> Result<Renewal, ReaderError> {
        let fs = &*self.cfg.fs;
        let path = &self.lease_path;
        let (f, found) = match fs.open(path, OpenMode::ReadWrite) {
            Ok(f) => {
                let b = read_upto(&*f, LEASE_LEN).map_err(io_err(path))?;
                (f, Some(b))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (
                fs.open(path, OpenMode::CreateNew).map_err(io_err(path))?,
                None,
            ),
            Err(e) => return Err(io_err(path)(e)),
        };
        let lost = self.image.is_some_and(|i| found.as_deref() != Some(&i[..]));
        let slot = LeaseSlot {
            epoch: if lost { epoch } else { self.lease_epoch(epoch) },
            refresh_counter: self.lease.refresh_counter + 1,
            highest_read: self.lease.highest_read.max(self.manifest.epoch),
            ..self.lease
        };
        // The slot alone, in place, when the header is the lease's own; else the whole file.
        let header = found
            .as_deref()
            .and_then(|b| b.get(..HEADER_LEN))
            .filter(|h| {
                FileHeader::decode(h)
                    .is_some_and(|h| h.kind == kind::LEASE && h.store_id == self.store_id)
            });
        let image = match header {
            Some(h) => {
                let mut image = [0u8; LEASE_LEN];
                image[..HEADER_LEN].copy_from_slice(h);
                image[SLOT_OFFSET..].copy_from_slice(&slot.encode());
                f.write_at(&image[SLOT_OFFSET..], SLOT_OFFSET as u64)
                    .map(|()| image)
            }
            None => {
                let image = slot.encode_file(self.store_id, generation);
                f.write_at(&image, 0).map(|()| image)
            }
        }
        .map_err(io_err(path))?;
        Ok(Renewal { slot, image, lost })
    }

    pub(super) fn commit_lease(&mut self, r: Renewal) {
        if r.lost {
            self.cuts.lose_all();
        }
        self.lease = r.slot;
        self.image = Some(r.image);
        self.renewed_at = self.cfg.clock.monotonic();
        self.state.lease_renewed_at = self.cfg.clock.wall();
        self.state.epoch = r.slot.epoch;
    }
}
