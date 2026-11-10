//! One try: which phase it belongs to, who asked for it, and what the shared
//! fact table said about the endpoint it would go to.
//!
//! [`Attempt`] is the value the shell hands the core around a send: `pre_send`
//! brackets it from above, `failed` from below, and the ledger projects the
//! pair into [`Facts`](super::verdict::Facts).
//!
//! Two tables meet here, named apart below: the shared upstream fact table
//! (`kallipai_common::facts`), which the shell reads into the attempt, and the
//! class table, which [`classify`](mod@super::classify) reads against the phase
//! a status landed in.

use std::time::Duration;

use kallipai_common::facts::BlockedFact;

use super::failure_class::{FailureClass, ShapeKey};

/// Which stretch of a call a reading was taken in: before any content flowed,
/// or after it.
///
/// `Connect`: nothing has flowed yet — a pre-send reading is taken in this phase
/// too, where the ledger alone decides. `MidStream`: content already flowed. The
/// shell decides the phase and injects it; one row of the class table reads it
/// (400/422, see [`classify`](super::classify::classify)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Connect,
    MidStream,
}

/// Where an attempt came from: the system (a retry, a re-run, a scheduled wake)
/// or an external wake — a user message, a task delivery, or an approval
/// notification.
///
/// Only the first attempt of a wake-triggered execution is `External`: a user
/// asking is served even when the shared table says the endpoint is done. Every
/// later send in that same execution is `System` again. The shell classifies the
/// attempt (the wake source lives there); the core only reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptOrigin {
    System,
    External,
}

/// Whether the chain has somewhere to go: a candidate endpoint the shell
/// resolved for this attempt.
///
/// The shell resolves it — it owns the chain and the fact table handle — and hands
/// over the first candidate that could serve where the current endpoint cannot:
/// one the shared table has nothing to say about. What "nothing to say" means is
/// the table's own keying, not a rule here: a fact names one profile, so it
/// answers for that profile alone. The core reads only that a candidate exists:
/// which one it is, and how many were skipped, stay in the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Alternative;

/// What the shared table says about one attempt: the fact the profile it would
/// go to already carries, and whether the chain has a candidate to offer
/// instead.
///
/// The shell resolves both from the table handle and the chain. `none` is the
/// reading an attempt carries when there is no table to read, or when the
/// reading was taken elsewhere: the pre-send reading is the one that carries
/// facts.
#[derive(Clone, Debug)]
pub(crate) struct TableReading {
    /// The readable fact for the profile the attempt would go to, if any.
    pub(crate) blocked: Option<BlockedFact>,
    /// The candidate the shell resolved, when one exists.
    pub(crate) alternative: Option<Alternative>,
}

impl TableReading {
    /// No table, or nothing to say: fail-open, so a reader that cannot reach
    /// the table behaves as if there were no facts.
    pub(crate) fn none() -> Self {
        Self {
            blocked: None,
            alternative: None,
        }
    }
}

/// One attempt's injected values: what the shell observed, in the shape the
/// verdict reads.
///
/// `failure: None` is the pre-send reading — nothing has failed yet, so the
/// verdict is about the ledger and the shared table alone. A settled failure
/// carries no table reading and no wake origin: its exit is the class rows'
/// business, and the reading belongs to the decision of whether to send at all.
#[derive(Clone, Debug)]
pub(crate) struct Attempt {
    pub(super) phase: Phase,
    pub(super) origin: AttemptOrigin,
    pub(super) failure: Option<FailureClass>,
    /// The shape key of this failure, when the error carries one. Computed by
    /// the shell (a shape is a property of the wire form), compared here.
    pub(super) shape: Option<ShapeKey>,
    pub(super) now: u64,
    pub(super) jitter: Duration,
    pub(super) reading: TableReading,
}

impl Attempt {
    /// An attempt that has not been sent: the pre-send reading, which carries
    /// what the table says about the endpoint it would go to.
    pub(crate) fn pre_send(
        phase: Phase,
        origin: AttemptOrigin,
        now: u64,
        reading: TableReading,
    ) -> Self {
        Self {
            phase,
            origin,
            failure: None,
            shape: None,
            now,
            jitter: Duration::ZERO,
            reading,
        }
    }

    /// The table's reading this attempt was built with: the pre-send
    /// verdict's own input, carried here rather than passed alongside.
    pub(crate) fn reading(&self) -> &TableReading {
        &self.reading
    }

    /// An attempt that failed: the class the table gave it, its shape key when
    /// it has one, and the values the shell injected.
    ///
    /// System-origin and no table reading: a failure that already happened is
    /// not the wake-triggered first attempt, and its exit is decided by the
    /// class rows rather than by whether a send should happen at all.
    pub(crate) fn failed(
        phase: Phase,
        failure: FailureClass,
        shape: Option<ShapeKey>,
        now: u64,
        jitter: Duration,
    ) -> Self {
        Self {
            phase,
            origin: AttemptOrigin::System,
            failure: Some(failure),
            shape,
            now,
            jitter,
            reading: TableReading::none(),
        }
    }
}
