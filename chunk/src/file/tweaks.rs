//! Departures from canonical writing ([`Tweaks`]).

use super::times::Batch;

/// Departures from canonical writing. `bodies` serves the canonical check, which hands the file's own data block
/// bodies back (a compressed XOR payload is a writer's choice, C.10); the other fields write files a v1 writer
/// never writes, for golden files and the reader's tests, and exist only with the `tweaks` feature.
#[derive(Clone, Debug, Default)]
pub struct Tweaks {
    /// Data block bodies in file order, each used where it is a canonical body of the same rows.
    pub bodies: Vec<Vec<u8>>,
    /// Bits set in every chunk's directory and index flags.
    #[cfg(feature = "tweaks")]
    pub chunk_flags: u16,
    /// A plain column added to the first data block.
    #[cfg(feature = "tweaks")]
    pub data_column: Option<(u8, Vec<u8>)>,
    /// A plain column added to the first commit-times block.
    #[cfg(feature = "tweaks")]
    pub times_column: Option<(u8, Vec<u8>)>,
    /// A field appended to the description.
    #[cfg(feature = "tweaks")]
    pub description_field: Option<(u16, Vec<u8>)>,
    /// Bytes appended to the trailer body.
    #[cfg(feature = "tweaks")]
    pub trailer_tail: Vec<u8>,
    /// Batch rows written in place of the held batches, unchecked.
    #[cfg(feature = "tweaks")]
    pub times_rows: Option<Vec<Batch>>,
    /// Leaves each chunk's last block out of its directory and index entry, though it is written.
    #[cfg(feature = "tweaks")]
    pub unrecorded_last_block: bool,
}

#[cfg(feature = "tweaks")]
impl Tweaks {
    pub(super) fn chunk_flags(&self) -> u16 {
        self.chunk_flags
    }
    pub(super) fn data_column(&self) -> Option<(u8, Vec<u8>)> {
        self.data_column.clone()
    }
    pub(super) fn take_times_column(&mut self) -> Option<(u8, Vec<u8>)> {
        self.times_column.take()
    }
    pub(super) fn description_field(&self) -> Option<(u16, Vec<u8>)> {
        self.description_field.clone()
    }
    pub(super) fn trailer_tail(&self) -> &[u8] {
        &self.trailer_tail
    }
    pub(super) fn take_times_rows(&mut self) -> Option<Vec<Batch>> {
        self.times_rows.take()
    }
    pub(super) fn unrecorded_last_block(&self) -> bool {
        self.unrecorded_last_block
    }
}

#[cfg(not(feature = "tweaks"))]
impl Tweaks {
    pub(super) fn chunk_flags(&self) -> u16 {
        0
    }
    pub(super) fn data_column(&self) -> Option<(u8, Vec<u8>)> {
        None
    }
    pub(super) fn take_times_column(&mut self) -> Option<(u8, Vec<u8>)> {
        None
    }
    pub(super) fn description_field(&self) -> Option<(u16, Vec<u8>)> {
        None
    }
    pub(super) fn trailer_tail(&self) -> &[u8] {
        &[]
    }
    pub(super) fn take_times_rows(&mut self) -> Option<Vec<Batch>> {
        None
    }
    pub(super) fn unrecorded_last_block(&self) -> bool {
        false
    }
}
