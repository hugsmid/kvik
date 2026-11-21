//! What a start takes beyond the configuration: the manifest's recorder, a listener for sealed segments, and the
//! generation a restore claimed (`spec.md` §3 Order of a start, §8 Generations).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use super::{LogWriter, OpenError, WriterConfig};
use crate::manifest::Recorder;
use crate::mark::ResetStamp;
use crate::segment::SealedFile;
use crate::setaside::SetAside;
use crate::store::StoreDir;

/// Told of each segment once it is sealed; called off the commit thread and must not block.
pub trait SealListener: Send + Sync {
    /// `file` is sealed and synced.
    fn sealed(&self, file: SealedFile);
}

/// The generation a start runs in, from the latest restore record (`spec.md` §8 Generations).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationStart {
    /// The generation.
    pub generation: u32,
    /// The restore's reset stamp: its sequence is the generation's first position.
    pub stamp: ResetStamp,
    /// The positions the restore and those before it set aside.
    pub set_aside: SetAside,
}

/// Options for [`LogWriter::open_with`]; the default is what [`LogWriter::open`] does.
#[derive(Default)]
pub struct StartOptions {
    /// The manifest's recorder; `None`: the store's `MANIFEST` file.
    pub recorder: Option<Box<dyn Recorder>>,
    /// Told of each sealed segment.
    pub sealed: Option<Arc<dyn SealListener>>,
    /// `None`: generation 0, nothing set aside.
    pub generation: Option<GenerationStart>,
}

impl fmt::Debug for StartOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartOptions")
            .field("recorder", &self.recorder.is_some())
            .field("sealed", &self.sealed.is_some())
            .field("generation", &self.generation)
            .finish()
    }
}

impl LogWriter {
    /// [`LogWriter::open`] with `opts`: in the generation `opts.generation` names, at the first position and with the
    /// reset stamp of its restore record (`spec.md` §8 Generations). `recorder` and `sealed` are refused with
    /// [`OpenError::NotSupportedYet`] rather than ignored until roadmap 6.3c and 6.3b provide what they call.
    pub fn open_with(
        store: StoreDir,
        cfg: WriterConfig,
        boot_id: Option<[u8; 16]>,
        now: Duration,
        wall: i64,
        opts: StartOptions,
    ) -> Result<Self, OpenError> {
        if opts.recorder.is_some() {
            return Err(OpenError::NotSupportedYet {
                what: "StartOptions::recorder",
            });
        }
        if opts.sealed.is_some() {
            return Err(OpenError::NotSupportedYet {
                what: "StartOptions::sealed",
            });
        }
        Self::start(store, cfg, boot_id, now, wall, opts)
    }
}
