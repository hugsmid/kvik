//! The read side of a durable store (Q-850 to Q-854, Q-900 to Q-904).

pub(crate) mod cut;
pub(crate) mod flight;
pub(crate) mod pmap;
#[cfg(test)]
mod tests;
pub(crate) mod view;
