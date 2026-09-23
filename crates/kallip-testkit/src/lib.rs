//! Test-support helpers shared across the workspace's crates.

use std::path::Path;

/// A guard directory under the managed `/tmp/kallipai-dev` root: the
/// `TempDir` deletes the tree on drop, so nothing outlives the run
/// (Deref/AsRef keep call sites reading as plain paths).
///
/// The root guard must live as long as the tree: dropping it
/// (as a local in new()) would delete the created paths.
pub struct DevDir(tempfile::TempDir);

impl DevDir {
    /// The guard's root path.
    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// Create a guard directory under the managed root, its name
    /// prefixed with `label`.
    pub fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join("kallipai-dev");
        std::fs::create_dir_all(&root).expect("create /tmp/kallipai-dev");
        Self(
            tempfile::Builder::new()
                .prefix(&format!("{label}-"))
                .tempdir_in(root)
                .expect("create test tempdir"),
        )
    }
}

impl std::ops::Deref for DevDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.0.path()
    }
}

impl AsRef<Path> for DevDir {
    fn as_ref(&self) -> &Path {
        self.0.path()
    }
}

/// Serialize environment-mutating tests process-wide: env edits are
/// visible to every concurrent test, so two tests editing the same
/// key would race. One lock serves both entry points (sync tests
/// take `blocking_lock`, async tests take `.lock().await`), so a
/// sync and an async test can never touch the environment at the
/// same time.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The undo log for an environment edit window. Restoring happens
/// in `Drop`, so a panicking closure unwinds through the restore:
/// the environment comes back clean even on the panic path.
///
/// Entries restore in reverse capture order, so a key captured twice
/// unwinds to its immediately prior value at each step (scope-exit
/// semantics), never to the oldest value in one jump.
struct EnvRestore<'a> {
    applied: Vec<(&'a str, Option<std::ffi::OsString>)>,
}

impl<'a> EnvRestore<'a> {
    fn capture(vars: &[(&'a str, Option<&str>)]) -> Self {
        let mut applied = Vec::new();
        for (key, value) in vars {
            applied.push((*key, std::env::var_os(key)));
            // SAFETY: the caller holds ENV_LOCK, so this thread is the
            // process-wide exclusive editor of the environment right now.
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        Self { applied }
    }
}

impl Drop for EnvRestore<'_> {
    fn drop(&mut self) {
        for (key, old) in self.applied.iter().rev() {
            // SAFETY: same exclusivity as `capture`: the lock guard
            // outlives this drop on every path, unwind included.
            match old {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }
}

/// Run `f` with the given variables set, restoring each key's prior
/// value afterwards (reverse order, so repeated keys unwind
/// step-wise). A variable mapped to `None` is removed for the
/// closure's duration. Sync tests only: the lock is taken with
/// `blocking_lock`, which panics inside an async runtime.
///
/// Not reentrant: calling `with_env` (or [`with_env_async`]) inside
/// a closure deadlocks on the non-reentrant mutex.
pub fn with_env<R>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
    let _guard = ENV_LOCK.blocking_lock();
    let _restore = EnvRestore::capture(vars);
    f()
}

/// Async counterpart of [`with_env`]: the lock is held across every
/// `.await`, so an async test whose body reads the environment stays
/// serialized for the whole window, not just the edit instant. Use
/// inside `#[tokio::test]` bodies; [`with_env`] would panic there.
/// Not reentrant, same as [`with_env`].
pub async fn with_env_async<R, F>(vars: &[(&str, Option<&str>)], f: F) -> R
where
    F: std::future::Future<Output = R>,
{
    let _guard = ENV_LOCK.lock().await;
    let _restore = EnvRestore::capture(vars);
    f.await
}

/// Poll `$cond` every 100ms for up to 5s (or an explicit bound) inside
/// an async context, then panic naming `$what`. The condition is
/// re-evaluated as written on every tick, so plain boolean expressions
/// and `.await`-ing ones both work, and it must be idempotent: no side
/// effects that change the verdict between ticks. Replaces
/// fixed-count retry loops whose caps silently under load: the bound
/// is wall-clock, and a timeout reports the wait object instead of
/// failing later on a downstream assertion. The two-argument form
/// uses the 5s default; the three-argument form takes a duration
/// first for waits that need more headroom.
#[macro_export]
macro_rules! wait_for {
    ($what:expr, $cond:expr) => {
        $crate::wait_for!(std::time::Duration::from_secs(5), $what, $cond)
    };
    ($dur:expr, $what:expr, $cond:expr) => {{
        let deadline = tokio::time::Instant::now() + $dur;
        loop {
            if $cond {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out after {:?} waiting for {}",
                $dur,
                $what
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_env_sets_values_and_restores_an_absent_key_to_absent() {
        with_env(&[("KALLIP_TESTKIT_PROBE", Some("inner"))], || {
            assert_eq!(
                std::env::var("KALLIP_TESTKIT_PROBE").as_deref(),
                Ok("inner")
            );
        });
        // The key was absent before the call, so the restore removes it.
        assert!(std::env::var_os("KALLIP_TESTKIT_PROBE").is_none());
    }

    #[test]
    fn with_env_restores_an_existing_value_exactly() {
        // SAFETY: this test is the only reader and writer of this probe
        // key anywhere in the suite; parallel tests never touch it.
        unsafe { std::env::set_var("KALLIP_TESTKIT_OUTER", "outer") };
        with_env(&[("KALLIP_TESTKIT_OUTER", Some("inner"))], || {
            assert_eq!(
                std::env::var("KALLIP_TESTKIT_OUTER").as_deref(),
                Ok("inner")
            );
        });
        assert_eq!(
            std::env::var("KALLIP_TESTKIT_OUTER").as_deref(),
            Ok("outer")
        );
        // SAFETY: same exclusivity as above; removing the key keeps the
        // starting state stable across repeated runs of the suite.
        unsafe { std::env::remove_var("KALLIP_TESTKIT_OUTER") };
    }

    #[test]
    fn with_env_restores_repeated_keys_in_reverse_order() {
        // SAFETY: exclusive probe key, as in the sibling tests.
        unsafe { std::env::set_var("KALLIP_TESTKIT_DUP", "first") };
        with_env(
            &[
                ("KALLIP_TESTKIT_DUP", Some("second")),
                ("KALLIP_TESTKIT_DUP", Some("third")),
            ],
            || {
                assert_eq!(std::env::var("KALLIP_TESTKIT_DUP").as_deref(), Ok("third"));
            },
        );
        // Reverse-order restore unwinds third -> second -> first; a
        // forward-order implementation would land on "second" instead.
        assert_eq!(std::env::var("KALLIP_TESTKIT_DUP").as_deref(), Ok("first"));
        unsafe { std::env::remove_var("KALLIP_TESTKIT_DUP") };
    }

    #[tokio::test]
    async fn with_env_async_holds_the_lock_and_restores_across_awaits() {
        // SAFETY: exclusive probe key, as in the sibling tests.
        unsafe { std::env::set_var("KALLIP_TESTKIT_ASYNC", "outer") };
        with_env_async(&[("KALLIP_TESTKIT_ASYNC", Some("inner"))], async {
            // A yield point inside the window: the lock must survive it.
            tokio::task::yield_now().await;
            assert_eq!(
                std::env::var("KALLIP_TESTKIT_ASYNC").as_deref(),
                Ok("inner")
            );
        })
        .await;
        assert_eq!(
            std::env::var("KALLIP_TESTKIT_ASYNC").as_deref(),
            Ok("outer")
        );
        unsafe { std::env::remove_var("KALLIP_TESTKIT_ASYNC") };
    }

    #[test]
    fn guard_creates_under_the_managed_root_and_cleans_up() {
        let dir = DevDir::new("testkit-selftest");
        let name = dir.0.path().file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("testkit-selftest-"));
        assert!(dir.path().is_dir());
        let path = dir.path().to_path_buf();
        assert!(path.exists());
        drop(dir);
        assert!(!path.exists(), "the guard must delete the tree on drop");
    }
}
