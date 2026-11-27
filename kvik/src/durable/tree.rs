//! The stored summary tree's writer (design 7.1b section 5): the dirty set the commit path marks, `dirty_from_log`,
//! and the checkpoint job on `kvik-maint`. The files themselves are `kvik_tree::write`'s.

pub(crate) mod dirty;
pub(crate) mod job;
pub(crate) mod stretch;
#[cfg(test)]
mod tests;

pub(crate) use dirty::Dirty;
pub(crate) use job::Job;
