//! Stateless, self-contained shell-command safety classifier.
//!
//! Encapsulated behind [`Classifier`] and returning its own [`Safety`] decision
//! type, this module depends only on the `rable` parser — it deliberately does
//! not import the runtime's `policy::ToolDecision`. The policy layer
//! ([`super::AgentPolicy`]) maps `Safety` to `ToolDecision` at a single boundary
//! (see [`crate::policy::agent`]).

mod catalog;
mod delegate;
mod helpers;
mod util;
mod walker;

#[cfg(test)]
mod tests;

use catalog::CommandSpec;

/// Authorization decision produced by the classifier.
///
/// Intentionally decoupled from [`super::ToolDecision`]: the classifier reasons
/// about *read-only-ness* of a shell command, not about the runtime's
/// allow/ask/deny enforcement. The policy layer maps these one-to-one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Safety {
    /// No mutation or code execution was detected.
    ReadOnly,
    /// The command may mutate state or execute code; defer to a human.
    NeedsApproval,
    /// The classifier declines to greenlight (e.g. unparseable input, or a
    /// hard-refused pattern such as piping a download into a shell).
    Reject { reason: String },
}

/// Encapsulates the read-only command catalog and classifies shell commands.
///
/// Owns its catalog so the policy that governs classification lives inside the
/// object rather than in a module global. The default catalog is the
/// compile-time [`catalog::READ_ONLY_CATALOG`]; a custom catalog can be supplied
/// for tests or a future per-agent policy.
#[derive(Clone, Copy, Debug)]
pub struct Classifier {
    catalog: &'static [CommandSpec],
}

impl Classifier {
    /// The default classifier, backed by the built-in read-only catalog.
    pub const DEFAULT: Self = Self {
        catalog: catalog::READ_ONLY_CATALOG,
    };

    /// Parse and classify a shell command. Fail-closed: unparseable input is
    /// [`Safety::Reject`].
    pub fn classify(&self, command: &str) -> Safety {
        walker::classify_command(self.catalog, command)
    }
}
