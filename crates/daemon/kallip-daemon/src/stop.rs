//! Stop semantics: read the pid from the data directory's runtime.json
//! (found through the record) → verify it is this instance's own live
//! tagma (the record's launch anchor or the name chain; the pid reuse
//! guard) → SIGTERM → poll for exit within the grace period → SIGKILL.
//! runtime.json stays (a daemon restart rebuilds its view from the
//! record area).
use std::path::Path;
use std::time::{Duration, Instant};

use crate::spawn::deny_guidance;
use kallip_daemon_common::wire::{ErrorCode, valid_slug};

#[derive(Debug, thiserror::Error)]
pub enum StopError {
    #[error("no instance named {0}")]
    NotFound(String),
    #[error("stop denied: slug {slug} is owned by uid {target_uid}; {}", deny_guidance(.target_uid))]
    Denied { slug: String, target_uid: u32 },
    #[error("instance {0} is not running (stale or missing pid)")]
    NotRunning(String),
    #[error("stop of {slug} failed after SIGKILL: {message}")]
    Internal { slug: String, message: String },
}

impl From<&StopError> for ErrorCode {
    fn from(error: &StopError) -> Self {
        match error {
            StopError::NotFound(_) => ErrorCode::NotFound,
            StopError::Denied { .. } => ErrorCode::Denied,
            StopError::NotRunning(_) => ErrorCode::NotRunning,
            StopError::Internal { .. } => ErrorCode::Internal,
        }
    }
}

const GRACE: Duration = Duration::from_secs(10);

/// Blocking stop. Identity is judged by `scan::identity_matches` — the
/// record's anchored pid/starttime, or the exe/comm name chain when the
/// claim point could not pin an anchor — so a recycled pid is refused.
pub fn stop(record_root: &Path, slug: &str, peer_uid: u32) -> Result<(), StopError> {
    // Same grammar gate as spawn/start: an invalid slug is not an
    // instance name, so it cannot exist — NotFound without a
    // record-area read (the slug must not become a path probe).
    if !valid_slug(slug) {
        return Err(StopError::NotFound(slug.to_string()));
    }
    let Some(record) = crate::records::read_record(record_root, slug) else {
        return Err(StopError::NotFound(slug.to_string()));
    };
    // Authorization precedes every state change. The target is the
    // recorded instance owner (the platform-hosting access rule):
    // the owner themselves or root may stop. A foreign peer is
    // refused with Denied, naming the slug and its owner: spawn's
    // SlugTaken already makes slug existence public, so the refusal
    // states its terms instead.
    if !crate::spawn::authorized(peer_uid, record.target_uid) {
        tracing::warn!(
            slug = %slug,
            peer_uid,
            target_uid = record.target_uid,
            "stop denied: foreign peer"
        );
        return Err(StopError::Denied {
            slug: slug.to_string(),
            target_uid: record.target_uid,
        });
    }
    let data_dir = record.data_dir.clone();
    let pid: u32 = crate::scan::read_runtime(&data_dir)
        .map(|runtime| runtime.pid)
        .ok_or_else(|| StopError::NotRunning(slug.to_string()))?;
    if crate::scan::identity_matches(&record, slug, pid) != crate::scan::Verdict::Match {
        tracing::warn!(
            slug = %slug,
            pid,
            comm = ?crate::scan::pid_comm(pid),
            exe = ?crate::scan::pid_exe(pid),
            "stop refused: recorded pid does not match this instance"
        );
        // Stale runtime state (crash leftover) or a recycled pid: the
        // instance behind it is gone; report it rather than shooting an innocent process.
        return Err(StopError::NotRunning(slug.to_string()));
    }

    send(pid, libc::SIGTERM).map_err(|m| internal(slug, m))?;
    tracing::info!(slug = %slug, pid, "stopping instance; sent SIGTERM");
    let deadline = Instant::now() + GRACE;
    while Instant::now() < deadline {
        if !alive(pid) {
            tracing::info!(slug = %slug, pid, "instance stopped");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    tracing::warn!(slug = %slug, pid, "grace period expired; escalating to SIGKILL");
    send(pid, libc::SIGKILL).map_err(|m| internal(slug, m))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !alive(pid) {
            tracing::info!(slug = %slug, pid, "instance stopped after SIGKILL");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    tracing::error!(slug = %slug, pid, "instance survived SIGKILL");
    Err(internal(slug, format!("pid {pid} survived SIGKILL")))
}

fn internal(slug: &str, message: String) -> StopError {
    StopError::Internal {
        slug: slug.to_string(),
        message,
    }
}

fn send(pid: u32, signal: i32) -> Result<(), String> {
    let rc = unsafe { libc::kill(pid as i32, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!(
            "kill({pid}, {signal}): {}",
            std::io::Error::last_os_error()
        ))
    }
}
/// A zombie counts as exited for our purposes: its /proc entry lingers
/// until reaped, so existence alone would over-report liveness.
fn alive(pid: u32) -> bool {
    crate::scan::pid_is_alive(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stop_denial_guidance_names_root_for_a_root_owned_record() {
        let error = StopError::Denied {
            slug: "mine".to_string(),
            target_uid: 0,
        };
        assert_eq!(
            error.to_string(),
            "stop denied: slug mine is owned by uid 0; run as root"
        );
    }

    #[test]
    fn stop_refuses_an_invalid_slug_before_touching_the_record_area() {
        let error = stop(Path::new("/nonexistent-records"), "../escape", unsafe {
            libc::getuid()
        })
        .unwrap_err();
        assert!(matches!(error, StopError::NotFound(_)), "{error}");
    }

    #[test]
    fn stop_refuses_a_peer_that_is_not_the_recorded_owner() {
        // The authorization verdict is a pure comparison against the
        // record, so the negative is exercisable without a second user:
        // record a foreign target uid, connect as the real one. The
        // refused peer is answered with Denied, which names the
        // slug and its owner instead of the missing-slug shape.
        let root = tempfile::tempdir().expect("record root");
        crate::records::write_record(
            root.path(),
            "mine",
            &crate::records::InstanceRecord {
                instance_id: "instance-1".into(),
                owner_uid: unsafe { libc::getuid() } + 1,
                target_uid: unsafe { libc::getuid() } + 1,
                target_username: None,
                workspace: Some("/ws".into()),
                env: vec![],
                identity: None,
                data_dir: root.path().join("data").join("i1"),
            },
        )
        .expect("write record");
        // A foreign peer, never root: root has the admin exemption, so
        // the negative needs a uid that is neither the target nor 0.
        let error = stop(root.path(), "mine", unsafe { libc::getuid() } + 2).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "stop denied: slug mine is owned by uid {}; run as the owner or root",
                unsafe { libc::getuid() } + 1
            )
        );
        // Root passes the same gate (the admin exemption), then
        // fails on the missing runtime.json like any authorized
        // caller.
        let error = stop(root.path(), "mine", 0).unwrap_err();
        assert!(matches!(error, StopError::NotRunning(_)), "{error}");
    }

    #[test]
    fn stop_allows_the_recorded_owner_but_only_that_peer() {
        let root = tempfile::tempdir().expect("record root");
        let uid = unsafe { libc::getuid() };
        crate::records::write_record(
            root.path(),
            "mine",
            &crate::records::InstanceRecord {
                instance_id: "instance-1".into(),
                owner_uid: uid,
                target_uid: uid,
                target_username: None,
                workspace: Some("/ws".into()),
                env: vec![],
                identity: None,
                data_dir: root.path().join("data").join("i1"),
            },
        )
        .expect("write record");
        // The owner passes authorization (and then fails on the
        // missing runtime.json — NotRunning). The ghost slug's
        // missing-slug no-op stays NotFound; a foreign peer on a
        // recorded slug is refused with Denied; root passes.
        let error = stop(root.path(), "ghost", uid + 1).unwrap_err();
        assert!(matches!(error, StopError::NotFound(_)), "{error}");
        let error = stop(root.path(), "mine", uid).unwrap_err();
        assert!(matches!(error, StopError::NotRunning(_)), "{error}");
        let error = stop(root.path(), "mine", uid + 1).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "stop denied: slug mine is owned by uid {}; {}",
                uid,
                deny_guidance(&uid)
            )
        );
    }
}
