//! A reader process's cuts (§7 Readers): each reads the files of one manifest epoch; the reader's lease names the
//! lowest epoch of its live cuts; a cut ends once it is `cut_max_age` old; a renewal that finds the lease gone ends
//! every cut of the reader first.

use super::{Error, FileId, Model, ReadResult, Reader, ResetStamp};
use crate::types::Nanos;

/// A cut a reader process holds (Q-903).
#[derive(Clone, Debug, PartialEq)]
pub struct Cut {
    /// Its id.
    pub id: u64,
    /// The reader's lease.
    pub lease: u64,
    /// The epoch whose files it reads.
    pub epoch: u64,
    /// The files listed at that epoch.
    pub files: Vec<FileId>,
    /// The reset stamp of the mark it was taken under.
    pub stamp: ResetStamp,
}

/// What the reader process keeps of a cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HeldCut {
    lease: u64,
    epoch: u64,
    taken_at: Nanos,
    /// Ended by a lease found gone.
    lost: bool,
}

impl Model {
    /// The reader of lease `r.lease` takes a cut at the latest epoch and renews its lease, which then names the
    /// lowest epoch of its live cuts. A lease found gone (closed, or one the writer would delete: of another boot,
    /// or expired) ends every cut of the reader first and is written again. `None` for a reader without a lease.
    pub fn take_cut(&mut self, r: &Reader) -> Option<Cut> {
        let id = r.lease?;
        if !self.lease_found(id) {
            self.lose_cuts(id);
            self.disk.leases.insert(
                id,
                super::Lease {
                    epoch: 0,
                    boot: self.boot,
                    counter: 0,
                },
            );
        }
        let epoch = self.disk.latest_epoch();
        self.next_cut += 1;
        let cut = self.next_cut;
        self.cuts.insert(
            cut,
            HeldCut {
                lease: id,
                epoch,
                taken_at: self.mono,
                lost: false,
            },
        );
        self.refresh_reader(r, true);
        Some(Cut {
            id: cut,
            lease: id,
            epoch,
            files: self.disk.listed_at(epoch),
            stamp: self.stamp(),
        })
    }

    /// Reads every file the cut lists, as [`Model::read`] does; [`Error::CutExpired`] once the cut is
    /// `cut_max_age` old, ended by a lease found gone, or released.
    pub fn read_cut(&self, c: &Cut) -> Result<ReadResult, Error> {
        if !self.cut_live(c.id) {
            return Err(Error::CutExpired);
        }
        self.read(&Reader {
            epoch: c.epoch,
            files: c.files.clone(),
            lease: Some(c.lease),
            stamp: c.stamp,
        })
    }

    /// The reader releases the cut; its lease may name a later epoch at the next renewal.
    pub fn end_cut(&mut self, c: &Cut) {
        self.cuts.remove(&c.id);
    }

    /// Whether the cut `id` is live: held, younger than `cut_max_age`, and not ended by a lease found gone.
    pub fn cut_live(&self, id: u64) -> bool {
        self.cuts.get(&id).is_some_and(|c| {
            !c.lost && self.mono.saturating_sub(c.taken_at) < self.config.cut_max_age
        })
    }

    /// The lowest epoch of the live cuts of lease `id`.
    pub fn lowest_cut(&self, id: u64) -> Option<u64> {
        self.cuts
            .iter()
            .filter(|(k, c)| c.lease == id && self.cut_live(**k))
            .map(|(_, c)| c.epoch)
            .min()
    }

    /// Whether a renewal finds lease `id` as the reader left it: present, and not one the writer would have deleted
    /// at its last look (§7 Leases). With the writer down, nothing deletes it.
    fn lease_found(&mut self, id: u64) -> bool {
        if !self.disk.leases.contains_key(&id) {
            return false;
        }
        if self.is_up() {
            let live = self.live_lease_ids();
            return live.contains(&id);
        }
        true
    }

    fn lose_cuts(&mut self, id: u64) {
        for c in self.cuts.values_mut().filter(|c| c.lease == id) {
            c.lost = true;
        }
    }
}
