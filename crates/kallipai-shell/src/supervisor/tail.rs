//! Tail helpers: bounded reads of a task's output, the interactive-prompt stall
//! patterns, and the capture lock the watcher and the registry share.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use regex::Regex;

use crate::backend::CaptureMode;
use crate::capture::BoundedCapture;
use crate::error::ShellError;

use super::STALL_TAIL;
use super::registry::TaskOutput;

/// Lock a shared capture, tolerating a poisoned lock (a pump that panicked
/// mid-`push`). The inner capture is still perfectly readable, and the
/// poison must not take the watcher down before `mark_terminal` — that
/// would leak the running-bg tally and permanently refuse this agent's
/// carves.
pub(crate) fn lock_cap(cap: &Arc<Mutex<BoundedCapture>>) -> MutexGuard<'_, BoundedCapture> {
    cap.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Stall tail probe per output source: the file's last [`STALL_TAIL`]
/// bytes, or an adopted task's rolling capture tail clipped to the same
/// window (the head and marker are deliberately not examined — only the
/// most recent output says what the command is waiting on).
pub(crate) fn tail_matches(output: &TaskOutput) -> bool {
    match output {
        TaskOutput::File { path } => tail_matches_prompt(path),
        TaskOutput::Pipes { out, err, mode } => {
            // Separate prompts may land on either stream, so probe both
            // tails (the same streams read_pipes_tail joins for reads).
            let tails: Vec<String> = match mode {
                CaptureMode::Merged | CaptureMode::Stdout => vec![lock_cap(out).tail_text()],
                CaptureMode::Stderr => vec![
                    lock_cap(err.as_ref().expect("stderr capture exists under Stderr")).tail_text(),
                ],
                CaptureMode::Separate => {
                    let e = err.as_ref().expect("stderr capture exists under Separate");
                    vec![lock_cap(out).tail_text(), lock_cap(e).tail_text()]
                }
            };
            tails
                .iter()
                .any(|t| stall_regex().is_match(&clip_tail(t.clone(), STALL_TAIL as usize)))
        }
    }
}

/// Interactive-prompt lockup patterns (kept conservative to avoid false positives).
fn stall_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?im)(password|passphrase|are you sure|confirm)").expect("valid regex")
    })
}

fn tail_matches_prompt(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(STALL_TAIL)));
    // take() bounds the read even while the file is being appended to:
    // read_to_end would pull the live appends into memory without bound
    // for as long as the writer keeps running.
    let mut buf = Vec::with_capacity(STALL_TAIL as usize);
    let _ = (&mut file).take(STALL_TAIL).read_to_end(&mut buf);
    stall_regex().is_match(&String::from_utf8_lossy(&buf))
}

pub(crate) fn read_tail(path: &Path, tail_bytes: usize) -> Result<String, ShellError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(tail_bytes as u64)));
    // take() bounds the read even if the file GROWS during the read: a live
    // task's appends after the seek point must never be pulled into memory.
    // Never read_to_end a task output file.
    let mut buf = Vec::with_capacity(tail_bytes);
    (&mut file).take(tail_bytes as u64).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Render an adopted task's captured output per `mode` (a live peek — the
/// streams may still be flowing) and clip to the last `tail_bytes`, the
/// same tail contract the file path honors. `Separate` joins the two
/// streams around a `[stderr]` divider: they were separate OS pipes all
/// along, so the true interleave never existed and this is a faithful
/// join, not a reconstruction.
pub(crate) fn read_pipes_tail(
    out: &Arc<Mutex<BoundedCapture>>,
    err: Option<&Arc<Mutex<BoundedCapture>>>,
    mode: CaptureMode,
    tail_bytes: usize,
) -> String {
    let peek = |cap: &Arc<Mutex<BoundedCapture>>, stream: &str| {
        crate::backend::with_banner(stream, &lock_cap(cap).peek())
    };
    let rendered = match mode {
        CaptureMode::Merged => peek(out, "output"),
        CaptureMode::Stdout => peek(out, "stdout"),
        CaptureMode::Stderr => peek(err.expect("stderr capture exists under Stderr"), "stderr"),
        CaptureMode::Separate => {
            let o = peek(out, "stdout");
            let e = peek(err.expect("stderr capture exists under Separate"), "stderr");
            if e.trim().is_empty() {
                o
            } else {
                format!("{o}\n[stderr]\n{e}")
            }
        }
    };
    clip_tail(rendered, tail_bytes)
}

/// Keep the last `tail_bytes` bytes of `s`, widening the cut to a char
/// boundary so no codepoint is sliced in half — the String twin of
/// `read_tail`'s byte-window clip.
fn clip_tail(mut s: String, tail_bytes: usize) -> String {
    if s.len() <= tail_bytes {
        return s;
    }
    let mut start = s.len() - tail_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s.split_off(start)
}

/// Unlink an adopted task's live spill files once nothing will reference
/// them again (the task is leaving the registry — no later banner can name
/// the files, and nothing else reads them).
pub(crate) fn remove_pipes_spill(output: &TaskOutput) {
    if let TaskOutput::Pipes { out, err, .. } = output {
        for cap in std::iter::once(out).chain(err.iter()) {
            if let Some(path) = lock_cap(cap).spill_path() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}
