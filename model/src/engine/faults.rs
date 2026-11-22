//! Faults injected into the model: a remount, power losses that lose
//! acknowledged batches, pages evicted after a failed write-back, a lost mark
//! or cursor file (§3 Recovery, §9).

use super::{ConsumerId, EntryBody, Error, FileId, MarkState, Model, WriteOutcome};

impl Model {
    /// The store's file system is unmounted and mounted again within the
    /// boot: the process stops and the page cache is lost, so the mark falls
    /// back to the last synced mark (§3 Recovery rule 1, row 7c's same boot).
    pub fn remount(&mut self) {
        self.remount_keeping(0);
    }

    /// A remount after which the first `keep` flagged batches not yet synced
    /// reached the disk (Q-250).
    pub fn remount_keeping(&mut self, keep: usize) {
        self.fast_signal(true);
        self.drop_page_cache_keeping(keep);
        self.crash();
    }

    /// While the engine is down on the boot of a failed sync: the pages whose
    /// write-back failed were marked clean and are reclaimed, so the batches
    /// beyond the mark read back as zeros; the mark's page, still dirty, is
    /// kept (§3 Recovery rule 3, Last batch written again; Q-254). Refused
    /// unless a log sync failed since the last start that synced.
    pub fn evict_failed_write_back(&mut self) -> Result<(), Error> {
        if self.vol.is_some() {
            return Err(Error::BadFault("stop the engine first"));
        }
        if !self.disk.write_back_failed {
            return Err(Error::BadFault("no failed write-back to evict"));
        }
        let mark = self.disk.mark;
        self.drop_page_cache_keeping(0);
        self.disk.mark = mark;
        Ok(())
    }

    /// A power loss after which the last `n` complete batches of the open
    /// segment are found invalid, at most those holding only facts beyond the
    /// last synced mark (all of them with the mark lost): the accepted loss
    /// of invariant 10, which can hold acknowledged and delivered batches.
    /// Returns how many were lost.
    pub fn power_loss_losing(&mut self, n: usize) -> usize {
        self.boot += 1;
        self.drop_page_cache();
        self.crash();
        let Some(active) = self.disk.active else {
            return 0;
        };
        let floor = match self.disk.synced_mark {
            Some(m) if m.file == active => m.batches,
            _ => 0,
        };
        let mut lost = 0;
        while lost < n {
            let file = self.disk.files.get_mut(&active).expect("segment");
            let len = file.batches.len();
            let eligible = len > floor
                && !self.disk.damaged.contains_key(&(active, len - 1))
                && file.batches[len - 1]
                    .iter()
                    .all(|e| matches!(e.body, EntryBody::Fact(_)))
                && !file.batches[len - 1].is_empty();
            if !eligible {
                break;
            }
            file.batches.pop();
            self.unflag_from(active, len - 1);
            lost += 1;
        }
        self.disk.invalid_tail.batches += u32::try_from(lost).unwrap_or(u32::MAX);
        lost
    }

    /// A power loss that keeps the first `keep` flagged batches not yet
    /// synced and loses the rest whole (§3 Recovery rule 3, Q-251).
    pub fn power_loss_keeping(&mut self, keep: usize) {
        self.fast_signal(true);
        self.boot += 1;
        self.drop_page_cache_keeping(keep);
        self.crash();
    }

    /// What the page cache held is gone: the batches written and not synced
    /// are lost, except as their write outcome says, and the mark falls back
    /// to the last synced mark.
    pub(super) fn drop_page_cache(&mut self) {
        self.drop_page_cache_keeping(0);
    }

    /// Beyond the page cache's mark lie flagged batches and at most one
    /// other batch, the last, whose write outcome is `tail_fate` (§3 Commit).
    /// The first `keep` of them reached the disk, all of them when the last
    /// survived; the model writes back in order (an earlier batch missing
    /// before a later one is rule 2, not modelled).
    fn drop_page_cache_keeping(&mut self, keep: usize) {
        if let Some(active) = self.disk.active.filter(|_| !self.disk.cache_lost) {
            let len = self.disk.files[&active].batches.len();
            let fate = self.disk.tail_fate;
            let floor = match self.disk.mark {
                Some(m) if m.file == active => m.batches.min(len),
                Some(_) => 0,
                None => len - usize::from(fate.is_some() && len > 0),
            };
            let n = len - floor;
            let survive = match fate {
                Some(WriteOutcome::Survived) => n,
                Some(_) => keep.min(n.saturating_sub(1)),
                None => keep.min(n),
            };
            if survive < n {
                self.disk
                    .files
                    .get_mut(&active)
                    .expect("segment")
                    .batches
                    .truncate(floor + survive);
                self.unflag_from(active, floor + survive);
                self.disk.invalid_tail.torn |= fate == Some(WriteOutcome::Torn) && survive + 1 == n;
            }
        }
        self.disk.tail_fate = None;
        self.disk.mark = self.disk.synced_mark;
        self.disk.cache_lost = true;
    }

    /// Forgets the flags of the batches of `file` from index `from` on, which
    /// are gone (§3 Batches).
    pub(super) fn unflag_from(&mut self, file: FileId, from: usize) {
        self.disk.flagged.retain(|(f, b)| *f != file || *b < from);
    }

    /// Fault injection while the engine is down: the mark file is missing or
    /// holds no valid slot (§3 Recovery rule 3, row 12a).
    pub fn lose_mark(&mut self, state: MarkState) -> Result<(), Error> {
        if self.vol.is_some() {
            return Err(Error::BadFault("stop the engine first"));
        }
        self.disk.mark = None;
        self.disk.synced_mark = None;
        self.disk.mark_damage = Some(state);
        Ok(())
    }

    /// Fault injection while the engine is down: a consumer's cursor file is
    /// missing or unreadable. The next recovery entry names it, and the
    /// consumer is treated as having no cursor (§3 Recovery rule 3).
    pub fn lose_cursor_file(&mut self, c: ConsumerId) -> Result<(), Error> {
        if self.vol.is_some() {
            return Err(Error::BadFault("stop the engine first"));
        }
        self.disk.cursors.remove(&c);
        self.disk.latches.retain(|(id, _), _| *id != c);
        self.disk.lost_cursors.insert(c);
        Ok(())
    }
}
