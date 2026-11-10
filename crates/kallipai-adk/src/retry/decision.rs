//! The retry decision core: failure classification, the episode's attempt
//! ledger, and the verdict one try gets.
//!
//! Pure value logic: no I/O, no clock reads, no RNG, no store, no tracing. The
//! shell — [`super::stream_with_retry`] and the acquisition loop — reads the
//! clock, takes the random source, and persists retry records; this module maps
//! already-typed error values onto a failure class, counts the episode's sends,
//! and reads both into a verdict.
//!
//! One duty per facet:
//!
//! - [`ledger`]: the episode's accounting — sends spent, shape slot, claimed window.
//! - [`attempt`]: what one try is — its phase, its origin, and the table
//!   reading it saw.
//! - [`failure_class`]: the class vocabulary, the code and retry kind each class is
//!   recorded under, and how many sends each class is worth.
//! - [`classify`](mod@classify): a typed error read into that vocabulary, provider
//!   evidence included.
//! - [`blocks`]: what a failure leaves in the shared upstream fact table.
//! - [`verdict`]: the facts read into one [`Decision`], and the wait it opens.

mod attempt;
mod blocks;
mod classify;
mod failure_class;
mod ledger;
mod verdict;

#[cfg(test)]
mod test_support;

pub use attempt::AttemptOrigin;
pub use ledger::Episode;

pub(crate) use attempt::{Alternative, Attempt, Phase, TableReading};
pub(crate) use blocks::fact_of;
pub(crate) use classify::{classify, shape_key};
pub(crate) use failure_class::{FailureClass, ShapeKey, TransientKind, retry_kind};
pub(crate) use verdict::{Decision, StopKind, decide};
