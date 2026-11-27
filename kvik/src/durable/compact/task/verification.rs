//! Verification of the written files a block at a time (compaction design §3.2), after the directory sync.

use kvik_chunk::file::{Position, SeriesInfo};

use super::super::run::{DATA_DIR, info};
use super::super::verify::Check;
use super::super::{Output, RunError};
use super::census::Census;
use super::{RunTask, Stage};

impl RunTask<'_> {
    /// Syncs the data directory and starts checking the files the passes wrote.
    pub(super) fn begin_verify(&mut self) -> Result<(), RunError> {
        self.input.fs.sync_dir(&self.input.dir.join(DATA_DIR))?;
        let plan = self.plan.take().expect("planned");
        let written = std::mem::take(&mut self.written);
        let ranges: Vec<(Position, Position)> =
            self.census.ranges.iter().map(|r| (r.0, r.1)).collect();
        self.census.expect.names = plan.names;
        self.census.batches = std::collections::BTreeMap::new();
        self.stage = Stage::Verify(Box::new(Check::new(written, &ranges)));
        Ok(())
    }

    /// One verification step; the outputs once every file has passed.
    pub(super) fn verify_step(&mut self) -> Result<Option<Vec<Output>>, RunError> {
        let Stage::Verify(check) = &mut self.stage else {
            unreachable!("verifying")
        };
        let Census { expect, defs, .. } = &mut self.census;
        let input = self.input;
        let infos = |s: u32, g: Position| -> SeriesInfo {
            let (name, reach) = info(&input, defs, s, g);
            SeriesInfo { name, reach }
        };
        match check.step(&input, expect, &infos)? {
            None => Ok(None),
            Some(out) => {
                debug_assert_eq!(out.len(), check.files().len());
                Ok(Some(out))
            }
        }
    }
}
