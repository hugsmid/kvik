//! The read side of a durable store (query design §2, §3).

pub(crate) mod cut;
pub(crate) mod flight;
pub(crate) mod pmap;
pub(crate) mod view;
