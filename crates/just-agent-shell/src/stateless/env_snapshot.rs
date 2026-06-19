//! One-time shell environment snapshot, replayed per call.
//!
//! The user's exports/functions/aliases are captured once at backend build into
//! a `0600` file (it may carry secrets) and `source`d by every command's
//! wrapper — giving the "feels like my shell" experience without a persistent
//! process. In-command `export`/`unset` do **not** persist across calls (only
//! this build-time snapshot does); that is the stateless semantics.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::ShellError;

/// Color-suppression env vars applied to every command (lifted from the PTY
/// backend) so tool output is free of escape sequences.
pub(super) const COLOR_VARS: &[(&str, &str)] = &[
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("LS_COLORS", ""),
    ("CLICOLOR", "0"),
];

/// Env-var name suffixes that look secret-bearing; matching `declare -x` lines
/// are dropped from the snapshot. Best-effort — an allowlist is the stronger
/// option (noted in the design doc).
const SECRET_SUFFIXES: &[&str] = &[
    "TOKEN",
    "SECRET",
    "KEY",
    "PASSWORD",
    "PASSPHRASE",
    "CREDENTIAL",
    "AUTH",
];

/// A captured shell environment, `source`d into each command's wrapper.
pub(super) struct EnvSnapshot {
    pub(super) path: PathBuf,
}

impl EnvSnapshot {
    /// Capture the current shell's exports, functions, and aliases into a
    /// `0600` file under `data_dir`. Run once at backend build.
    pub(super) fn capture(data_dir: &Path, shell: OsString) -> Result<Self, ShellError> {
        let path = data_dir.join("env.sh");
        // `shopt -s expand_aliases` must be in effect when aliases are defined
        // here AND when they are used in the per-call wrapper.
        let script = "shopt -s expand_aliases\ndeclare -px\ndeclare -fx\nalias\n";
        let output = run_capture(shell, script)?;
        let body = scrub(&output);
        write_private(&path, body.as_bytes())?;
        Ok(Self { path })
    }
}

/// Run `<shell> -c '<script>'` and capture stdout.
fn run_capture(shell: OsString, script: &str) -> Result<String, ShellError> {
    let output = std::process::Command::new(&shell)
        .arg("-c")
        .arg(script)
        .output()
        .map_err(|e| ShellError::env_snapshot(format!("capture failed: {e}")))?;
    if !output.status.success() {
        return Err(ShellError::env_snapshot(format!(
            "snapshot script exited {}",
            output.status.code().unwrap_or(-1)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Drop exported-variable declarations whose name looks secret-bearing.
///
/// Only single-line `declare -x NAME=...` / `export NAME=...` lines are filtered;
/// function definitions and aliases pass through untouched (filtering inside a
/// multiline function body would corrupt it).
fn scrub(decls: &str) -> String {
    decls
        .lines()
        .filter(|line| !looks_secret(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn looks_secret(line: &str) -> bool {
    let trimmed = line.trim_start();
    let is_export =
        (trimmed.starts_with("declare") && trimmed.contains('=')) || trimmed.starts_with("export ");
    if !is_export {
        return false;
    }
    let name = trimmed
        .split_whitespace()
        .find(|tok| tok.contains('='))
        .and_then(|tok| tok.split('=').next())
        .unwrap_or("");
    let upper = name.to_uppercase();
    SECRET_SUFFIXES.iter().any(|s| upper.ends_with(s))
}

// Unlike `pgroup` (whose non-unix path was dead weight and was removed), this
// non-unix `write_private` is a *live graceful fallback* — the file is still
// written, just without `0600`. Keep it; don't "tidy" it away.
#[cfg(unix)]
fn write_private(path: &Path, body: &[u8]) -> Result<(), ShellError> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(body)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, body: &[u8]) -> Result<(), ShellError> {
    std::fs::write(path, body).map_err(ShellError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_drops_secret_exports() {
        let decls =
            "declare -x PATH=\"/usr/bin\"\ndeclare -x API_TOKEN=\"abc\"\nalias ll='ls -l'\n";
        let scrubbed = scrub(decls);
        assert!(scrubbed.contains("PATH"));
        assert!(!scrubbed.contains("API_TOKEN"));
        assert!(scrubbed.contains("alias ll")); // aliases untouched
    }

    #[test]
    fn scrub_leaves_function_bodies_intact() {
        // A function body referencing a secret-named local must NOT be filtered.
        let decls = "myfn () \n{ \n    local API_TOKEN=\"x\"\n}\n";
        let scrubbed = scrub(decls);
        assert!(scrubbed.contains("local API_TOKEN"));
    }

    #[test]
    fn looks_secret_cases() {
        assert!(looks_secret("declare -x API_TOKEN=\"x\""));
        assert!(looks_secret("export MY_KEY=value"));
        assert!(!looks_secret("declare -x PATH=\"/usr/bin\""));
        assert!(!looks_secret("alias ll='ls -l'"));
        assert!(!looks_secret("myfn () { local SECRET=1; }"));
    }
}
