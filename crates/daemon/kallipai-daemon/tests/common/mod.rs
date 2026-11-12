//! Scaffolding shared by the daemon's end-to-end tests.

use std::path::Path;

/// Refuse a boot whose instance roots can resolve outside `sandbox`.
///
/// The check reads the child's own environment back instead of trusting each
/// test to pin every home, then hands the pairs to the one sandbox guard in
/// `kallipai-testkit`: the own root keys must be absent (one inherited from the
/// caller rides the deployment layer into every launch) and `HOME`, plus any
/// `XDG_*` that is set, must resolve inside the sandbox.
pub fn assert_sandboxed(pid: u32, sandbox: &Path) {
    kallipai_testkit::assert_process_roots(pid, &[sandbox]);
}
