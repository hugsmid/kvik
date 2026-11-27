//! The stored summary tree (design 7.1b): the dirty set the commit path marks, `dirty_from_log`, the checkpoint job on
//! `kvik-maint`, and the tree a reader takes with its cut. The files themselves are `kvik_tree`'s.

pub(crate) mod dirty;
pub(crate) mod job;
pub(crate) mod read;
#[cfg(test)]
mod reads;
#[cfg(test)]
mod rebuild_tests;
pub(crate) mod stretch;
#[cfg(test)]
mod tests;

pub(crate) use dirty::Dirty;
pub(crate) use job::Job;
