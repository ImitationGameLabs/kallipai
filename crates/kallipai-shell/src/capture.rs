//! Bounded head+tail capture for one stdout/stderr stream.
//!
//! Keeps a frozen head (the first `head_budget` bytes) and a rolling tail (the
//! last `tail_budget` bytes), `head_budget + tail_budget = max_bytes`, so a
//! runaway command can't exhaust memory while the most informative parts of the
//! output (the start and the end) stay visible. When the total exceeds
//! `max_bytes`, the middle is dropped from the in-memory view and the
//! [`CaptureResult::truncated`] flag is set.
//!
//! The dropped middle is not lost: on the *first* overflow [`BoundedCapture`]
//! lazily spills the complete stream (head + middle + tail) to a file under
//! `spill_dir`, so the caller can surface its path and the agent can `Read` the
//! full output back. Under-budget commands create no file and pay no spill I/O.

use std::path::PathBuf;

use crate::spill::{BASH_EXEC_SPILL, StreamingSpill};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The disk-budget pool shared across a task's streams, plus the outcome of
/// feeding one chunk to a capture.

#[derive(Clone)]
pub(super) struct DiskQuota {
    used: Arc<AtomicUsize>,
    cap: usize,
}

impl DiskQuota {
    /// A fresh pool with a `cap`-byte total disk budget.
    pub(super) fn new(cap: usize) -> Self {
        Self {
            used: Arc::new(AtomicUsize::new(0)),
            cap,
        }
    }

    /// Pre-write reservation: succeed only if the total stays within cap.
    /// The CAS loop keeps the bound exact no matter how many pumps race
    /// (both streams of one exec, or a converted task's capture pump
    /// alongside its log pump); a refused reservation writes nothing, and
    /// the pump stops on the first refusal.
    pub(super) fn try_reserve(&self, n: usize) -> bool {
        let mut seen = self.used.load(Ordering::Relaxed);
        loop {
            if seen.saturating_add(n) > self.cap {
                return false;
            }
            match self.used.compare_exchange_weak(
                seen,
                seen + n,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => seen = now,
            }
        }
    }

    /// Hand reserved bytes back after a failed spill open or write, so a
    /// sibling stream is not short-changed by a dead consumer.
    pub(super) fn release(&self, n: usize) {
        self.used.fetch_sub(n, Ordering::Relaxed);
    }

    pub(super) fn cap(&self) -> usize {
        self.cap
    }

    /// Bytes currently reserved against the pool: the volume actually
    /// written to disk across both streams.
    pub(super) fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }
}

impl Default for DiskQuota {
    /// Placeholder pool for `mem::take` on a finalized capture; the budget
    /// of a taken capture is never consulted again.
    fn default() -> Self {
        Self::new(usize::MAX)
    }
}

/// Outcome of appending one chunk to a capture.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PushOutcome {
    /// Chunk fully recorded; keep pumping.
    Flowing,
    /// Disk cap reached: stop reading and close the pipe read end.
    Capped,
}

/// A bounded head+tail collector for one stream.
///
/// The `head` fills first and is frozen once it reaches `head_budget`; later
/// bytes roll through `tail`, which keeps only the most recent `tail_budget`
/// bytes. `total` tracks all bytes seen so overflow can be detected even after
/// the middle has been dropped.
#[derive(Default)]
pub(super) struct BoundedCapture {
    max_bytes: usize,
    head_budget: usize,
    tail_budget: usize,
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
    /// Lazy full-stream spill. `Closed` until the first overflow; `Open` once the
    /// file is created and every later chunk is appended; `Poisoned` (terminal)
    /// if the file could not be created or a mid-stream write failed, so the
    /// failing `open`/`write` is never retried for a long overflowing command.
    spill: SpillState,
    /// Per-exec uuid nonce plus the stream label: the pair keys the in-flight
    /// spill temp name so concurrent execs and the two streams of one exec
    /// never collide.
    nonce: String,
    /// Stream label (`merged`, `stdout`, or `stderr`; the backend chooses per
    /// capture mode), combined with the nonce into the spill stream key.
    stream_label: &'static str,
    /// Root of the bash-exec spill family the temp file is created under
    /// (a landlocked-readable directory).
    spill_dir: PathBuf,
    /// Task-wide disk budget shared with the sibling stream: the accounting
    /// pool spans both captures of one exec, so the combined spill stays
    /// within the cap regardless of per-stream construction.
    quota: DiskQuota,
    /// Set once the pool refused a write; the pump closes the pipe read end
    /// and the reason is composed at finish/peek time.
    capped: bool,
}

/// Spill-file lifecycle for [`BoundedCapture::spill`].
#[derive(Default)]
enum SpillState {
    /// Not yet overflowing; no file created.
    #[default]
    Closed,
    /// Overflowing; the file is open and being appended.
    Open(StreamingSpill),
    /// Spill failed irrecoverably; degrade to head+tail view with no path.
    Poisoned,
}

/// The finalized capture of one stream.
#[derive(Debug, Default, Clone)]
pub(super) struct CaptureResult {
    /// The in-memory view: the full output when it fit, otherwise
    /// `head + "[... N bytes omitted ...]" + tail` (lossily decoded).
    pub text: String,
    /// `true` if `total` exceeded `max_bytes` (the middle was dropped).
    pub truncated: bool,
    /// Absolute path to the spill file holding the COMPLETE stream, present only
    /// when this stream overflowed AND the spill file is healthy. `None` for
    /// under-budget streams or a poisoned (failed) spill.
    pub spill: Option<PathBuf>,
    /// The nonce-unique `.tmp-` twin of the spill file while it was open,
    /// present whenever this stream opened a spill. This — not the shared
    /// content-addressed name — is the only path a discard cleanup may
    /// safely unlink: unlinking the content name would pull the file out
    /// from under every other banner that resolved to the same bytes.
    pub tmp_twin: Option<PathBuf>,
    /// Present when the task-wide disk cap refused a write: the human-facing
    /// termination reason (original byte count + recovery guidance), carried
    /// through read/finish so the agent sees why the stream stopped.
    pub killed_reason: Option<String>,
}

/// Render a byte budget for the reason line: whole MiB/KiB when exact,
/// plain bytes otherwise, so a sub-MiB cap never reads as "0 MiB".
pub(super) fn fmt_cap(cap: usize) -> String {
    const MIB: usize = 1024 * 1024;
    const KIB: usize = 1024;
    if cap >= MIB && cap.is_multiple_of(MIB) {
        format!("{} MiB", cap / MIB)
    } else if cap >= KIB && cap.is_multiple_of(KIB) {
        format!("{} KiB", cap / KIB)
    } else {
        format!("{cap} bytes")
    }
}

/// Termination reason for a refused write, shared by the foreground capture
/// and the background log pump so both faces carry identical wording: the
/// cap, the bytes that reached disk, and the recovery path.
pub(super) fn disk_cap_reason(quota: &DiskQuota) -> String {
    format!(
        "output exceeded the {} disk cap after {} bytes written to disk; writes terminated (SIGPIPE/EPIPE) — narrow the command or redirect its output to a file",
        fmt_cap(quota.cap()),
        quota.used()
    )
}

impl BoundedCapture {
    /// Creates a collector that retains a head of `max_bytes/2` and a tail of
    /// the remainder, spilling the full stream to `spill_dir` on overflow.
    ///
    /// `quota` is the per-task disk budget (one pool per task, never shared
    /// across tasks): both streams of one exec (and a converted task's
    /// continued pumping) draw from it, so the combined spilled bytes stay
    /// within the cap no matter how the captures are constructed.
    pub(super) fn new(
        max_bytes: usize,
        nonce: &str,
        stream_label: &'static str,
        spill_dir: PathBuf,
        quota: DiskQuota,
    ) -> Self {
        let head_budget = max_bytes / 2;
        let tail_budget = max_bytes - head_budget;
        Self {
            max_bytes,
            head_budget,
            tail_budget,
            head: Vec::new(),
            tail: Vec::new(),
            total: 0,
            spill: SpillState::default(),
            nonce: nonce.to_owned(),
            stream_label,
            spill_dir,
            quota,
            capped: false,
        }
    }

    /// Append a chunk: lazily open the spill on the first overflow, append to it
    /// thereafter, and feed the in-memory head (until frozen) then the rolling
    /// tail. The spill-flush ordering is load-bearing: the head+tail prefix is
    /// flushed *before* the overflowing chunk is appended, so the file ends up
    /// byte-identical to the true stream.
    ///
    /// Returns [`PushOutcome::Capped`] when the task-wide disk budget refused
    /// a write: the refused chunk is neither added to the in-memory view nor
    /// spilled (it counts toward the running total only), and the caller must
    /// stop reading and close the pipe read end — the child's next write then
    /// dies on SIGPIPE (or fails with EPIPE), so the on-disk total stays
    /// strictly within the cap (pre-write check).
    pub(super) fn push(&mut self, chunk: &[u8]) -> PushOutcome {
        self.total += chunk.len();
        let will_overflow = self.total > self.max_bytes;

        // Lazy spill: open on the FIRST overflow, flushing the in-memory view
        // (head + tail = the complete prefix so far, <= max_bytes) so the file
        // ultimately holds the entire stream. The flush itself spends the
        // shared disk budget; a refusal here means the disk cap is sized below
        // the memory budget — degrade to capped without ever opening a file.
        if will_overflow && matches!(self.spill, SpillState::Closed) {
            let prefix = self.head.len() + self.tail.len();
            if !self.quota.try_reserve(prefix) {
                self.capped = true;
                return PushOutcome::Capped;
            }
            match self.open_streaming_spill() {
                Ok(handle) => self.spill = SpillState::Open(handle),
                // Open failure after a successful reservation: hand the bytes
                // back so a sibling stream is not short-changed.
                Err(_) => {
                    self.quota.release(prefix);
                    self.spill = SpillState::Poisoned;
                }
            }
        }
        // Every chunk after the spill opens is appended, so the file is complete.
        if let SpillState::Open(handle) = &mut self.spill {
            if !self.quota.try_reserve(chunk.len()) {
                self.capped = true;
                return PushOutcome::Capped;
            }
            if handle.append(chunk).is_err() {
                // Mid-stream write failure: stop spilling and keep what we
                // have; surface no path so the caller never points at a
                // partial file. The reservation goes back to the pool.
                self.quota.release(chunk.len());
                self.spill = SpillState::Poisoned;
            }
        }

        // In-memory: fill the (frozen once full) head, then the rolling tail.
        let mut rest = chunk;
        if self.head.len() < self.head_budget {
            let take = (self.head_budget - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        if !rest.is_empty() {
            self.tail.extend_from_slice(rest);
            if self.tail.len() > self.tail_budget {
                let start = self.tail.len() - self.tail_budget;
                self.tail.drain(0..start);
            }
        }
        PushOutcome::Flowing
    }

    /// Open the streaming spill, writing the current head+tail prefix
    /// first. The caller then appends each subsequent chunk via the `Open`
    /// arm of `push`.
    ///
    /// The safe-write discipline (0700 directory chain, O_NOFOLLOW dir
    /// open, O_EXCL leaf) lives in spill::StreamingSpill — single-sourced
    /// with the message-entry spill so the two faces cannot drift. The dir
    /// is created lazily here, so under-budget captures write nothing. The
    /// temp name survives finalize's content-addressed link on purpose:
    /// banners that named the in-flight file (peek while a converted
    /// background task is still running) must keep resolving.
    fn open_streaming_spill(&self) -> std::io::Result<StreamingSpill> {
        let mut spill = StreamingSpill::create(
            &self.spill_dir,
            &BASH_EXEC_SPILL,
            &format!("{}-{}", self.nonce, self.stream_label),
        )?;
        spill.append(&self.head)?;
        spill.append(&self.tail)?;
        Ok(spill)
    }

    /// Render the current in-memory view, shared by `finish` and `peek`: the
    /// full contiguous output under budget, else head + middle-omitted marker
    /// + tail. Returns `(text, truncated)`.
    fn render_view(&self) -> (String, bool) {
        let truncated = self.total > self.max_bytes;
        let text = if !truncated {
            // No overflow: head + tail is the full, contiguous output. Decode the
            // concatenation as one buffer so a head/tail split landing mid-codepoint
            // does not synthesize a spurious replacement character.
            let mut combined = Vec::with_capacity(self.head.len() + self.tail.len());
            combined.extend_from_slice(&self.head);
            combined.extend_from_slice(&self.tail);
            String::from_utf8_lossy(&combined).into_owned()
        } else {
            let omitted = self.total - self.head.len() - self.tail.len();
            let mut text = String::with_capacity(self.head.len() + self.tail.len() + 48);
            text.push_str(&String::from_utf8_lossy(&self.head));
            text.push_str(&format!("\n[... {omitted} bytes omitted ...]\n"));
            text.push_str(&String::from_utf8_lossy(&self.tail));
            text
        };
        (text, truncated)
    }

    /// Finalize into a [`CaptureResult`], rendering the head+tail view (with a
    /// middle-omitted marker on overflow) and surfacing the spill path when
    /// the spill is healthy. Finalizing links the temp file to its
    /// content-addressed name (see spill::StreamingSpill); the surfaced path
    /// is that content-addressed one, while earlier peek banners keep
    /// resolving through the surviving temp name.
    pub(super) fn finish(mut self) -> CaptureResult {
        let (text, truncated) = self.render_view();
        // Take the twin before the handle is consumed; it exists once the
        // spill opened, whether or not finalize later succeeds.
        let tmp_twin = match &self.spill {
            SpillState::Open(handle) => Some(handle.tmp_path().to_path_buf()),
            _ => None,
        };
        let spill = match std::mem::replace(&mut self.spill, SpillState::Closed) {
            SpillState::Open(handle) => match handle.finalize() {
                Ok(path) => Some(path),
                // A finalize failure (e.g. a pre-occupied content hash with
                // different bytes) degrades to the head+tail view with no
                // path, exactly like a failed open mid-stream.
                Err(e) => {
                    tracing::warn!("bash-exec spill finalize failed: {e}");
                    None
                }
            },
            _ => None,
        };
        CaptureResult {
            text,
            truncated,
            spill,
            tmp_twin,
            killed_reason: self.cap_reason(),
        }
    }

    /// Snapshot the exact view [`Self::finish`] would render, WITHOUT
    /// consuming the capture. Used while the stream is still being pumped
    /// (a timed-out foreground exec converted to a background task): bytes
    /// keep arriving after this peek, so later peeks see more. The spill
    /// path is surfaced when healthy, so a caller-side recovery banner
    /// keeps naming a file that is still being appended to.
    pub(super) fn peek(&self) -> CaptureResult {
        let (text, truncated) = self.render_view();
        let spill = self.spill_path();
        let tmp_twin = self.spill_path();
        CaptureResult {
            text,
            truncated,
            spill,
            tmp_twin,
            killed_reason: self.cap_reason(),
        }
    }

    /// The termination reason when the disk cap refused a write: names the
    /// cap, the captured volume, and the recovery path (narrow the command
    /// or redirect to a file). `None` while the capture flowed freely.
    fn cap_reason(&self) -> Option<String> {
        self.capped.then(|| disk_cap_reason(&self.quota))
    }

    /// Path of the live spill file when this capture has overflowed and the
    /// spill is healthy (`None` under budget or poisoned). For cleanup when
    /// the owning task is discarded without a `finish` (a converted task
    /// killed or dropped), so an unreferenced spill file does not leak on
    /// disk: without this it would outlive its last banner.
    pub(super) fn spill_path(&self) -> Option<PathBuf> {
        match &self.spill {
            SpillState::Open(handle) => Some(handle.tmp_path().to_path_buf()),
            _ => None,
        }
    }

    /// Total bytes seen so far — monotonic even after the rendered view
    /// clips (the head freezes and the tail rolls). The size watchdog and
    /// `bytes` counter of a converted background task read this so clipping
    /// never freezes them.
    pub(super) fn total_bytes(&self) -> usize {
        self.total
    }

    /// The rolling tail's current bytes, lossily decoded (at most
    /// `tail_budget`). Stall detection runs its prompt regex over just this
    /// — the most recent output — mirroring the file task's STALL_TAIL
    /// window instead of the whole head+marker+tail view.
    pub(super) fn tail_text(&self) -> String {
        String::from_utf8_lossy(&self.tail).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// A scratch dir that isolates spill files to the test and cleans them on drop.
    fn scratch() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    fn cap(budget: usize, dir: &tempfile::TempDir) -> BoundedCapture {
        BoundedCapture::new(
            budget,
            "nonce",
            "out",
            dir.path().join("spill"),
            DiskQuota::new(usize::MAX),
        )
    }

    #[test]
    fn under_budget_keeps_full_output_and_creates_no_file() {
        let dir = scratch();
        let mut c = cap(10, &dir);
        c.push(b"hello");
        let r = c.finish();
        assert_eq!(r.text, "hello");
        assert!(!r.truncated);
        assert!(r.spill.is_none());
        assert!(dir.path().read_dir().unwrap().next().is_none());
    }

    #[test]
    fn overflow_renders_head_marker_tail_and_spills_full_stream() {
        let dir = scratch();
        // head_budget = 4, tail_budget = 4. Push bytes before AND after the
        // overflow boundary to exercise head-flush-then-append.
        let mut c = cap(8, &dir);
        c.push(b"ab"); // head
        c.push(b"cd"); // head fills to 4
        c.push(b"ef"); // total=6, no overflow yet -> tail
        c.push(b"gh"); // total=8, still == max, no overflow
        c.push(b"ij"); // total=10 > 8 -> overflow: spill flushes "abcdefgh", appends "ij"
        c.push(b"kl"); // total=12 -> spill appends "kl"
        let r = c.finish();
        assert!(r.truncated);
        // head = "abcd", tail = last 4 of "ijkl" joined -> "ijkl"
        assert!(r.text.contains("abcd"), "head present: {}", r.text);
        assert!(r.text.contains("ijkl"), "tail present: {}", r.text);
        assert!(
            r.text.contains("bytes omitted"),
            "middle-omitted marker: {}",
            r.text
        );
        let omitted = 12 - 4 - 4; // total - head - tail
        assert!(r.text.contains(&format!("{omitted} bytes omitted")));
        // The spill file holds the COMPLETE stream.
        let path = r.spill.expect("spill path");
        let spilled = std::fs::read(&path).unwrap();
        assert_eq!(&spilled[..], b"abcdefghijkl", "spill is byte-identical");
    }

    #[test]
    fn spill_byte_identical_across_many_chunks() {
        let dir = scratch();
        let mut c = cap(7, &dir); // head 3, tail 4
        let mut full = Vec::new();
        for i in 0..50u8 {
            let chunk = [i, i, i]; // 3 bytes each
            c.push(&chunk);
            full.extend_from_slice(&chunk);
        }
        let r = c.finish();
        assert!(r.truncated);
        let spilled = std::fs::read(r.spill.as_ref().unwrap()).unwrap();
        assert_eq!(spilled, full, "spill equals the true stream");
        // head frozen at the first 3 bytes.
        assert!(r.text.as_bytes().starts_with(&[0u8, 0, 0]));
    }

    #[test]
    fn unreachable_spill_dir_poisons_and_keeps_tail() {
        // A spill_dir whose parent does not exist: create_new fails -> Poisoned.
        let mut c = BoundedCapture::new(
            4,
            "nonce",
            "out",
            PathBuf::from("/nonexistent-kallipai-test-dir-xyz"),
            DiskQuota::default(),
        );
        c.push(b"ab");
        c.push(b"cdefgh"); // overflow: open fails -> Poisoned; later bytes still buffered
        let r = c.finish();
        assert!(r.truncated);
        assert!(r.spill.is_none(), "poisoned spill surfaces no path");
        // head + tail view still rendered.
        assert!(r.text.contains("bytes omitted"));
    }

    #[test]
    fn spill_state_default_is_closed() {
        let s = SpillState::default();
        assert!(matches!(s, SpillState::Closed));
    }

    #[test]
    fn under_budget_concatenates_head_and_tail_without_split_artifact() {
        // Output that straddles the head/tail boundary mid-multibyte char must
        // not synthesize a replacement char when it all fits.
        let dir = scratch();
        let mut c = cap(8, &dir);
        let s = "héllo"; // 6 bytes
        c.push(s.as_bytes());
        let r = c.finish();
        assert!(!r.truncated);
        assert_eq!(r.text, "héllo");
    }

    #[test]
    fn peek_matches_later_finish_and_keeps_capture_alive() {
        let dir = scratch();
        let mut c = cap(8, &dir);
        c.push(b"abcd");
        let mid = c.peek();
        assert_eq!(mid.text, "abcd");
        assert!(!mid.truncated);
        // peek does not consume: more bytes still land.
        c.push(b"efgh");
        let r = c.finish();
        assert_eq!(r.text, "abcdefgh");
    }

    #[test]
    fn peek_on_overflow_surfaces_spill_and_marker() {
        let dir = scratch();
        let mut c = cap(8, &dir); // head 4, tail 4
        c.push(b"abcdefghijkl"); // overflow: spill holds the full stream
        let p = c.peek();
        assert!(p.truncated);
        assert!(p.text.contains("bytes omitted"));
        let path = p.spill.as_ref().expect("live spill surfaced");
        assert_eq!(std::fs::read(path).unwrap(), b"abcdefghijkl");
        // The live capture still exposes the same spill path for cleanup.
        assert_eq!(c.spill_path().as_ref(), Some(path));
        // And finishing later still works (peek duplicated nothing).
        let r = c.finish();
        assert_eq!(
            std::fs::read(r.spill.as_ref().unwrap()).unwrap(),
            b"abcdefghijkl"
        );
    }

    #[test]
    fn total_bytes_is_monotonic_and_tail_text_tracks_recent_output() {
        let dir = scratch();
        let mut c = cap(16, &dir); // head 8, tail 8
        c.push(b"0123456789");
        assert_eq!(c.total_bytes(), 10);
        assert_eq!(c.tail_text(), "89");
        c.push(b"abcdefghij");
        assert_eq!(c.total_bytes(), 20);
        assert_eq!(c.tail_text(), "cdefghij");
    }

    /// After finish, the spill's content-addressed name sits under the
    /// family's 2-hex shard, and the .tmp twin survives under the same
    /// inode: earlier banners naming the in-flight path keep resolving.
    #[test]
    fn finish_links_content_addressed_name_and_keeps_tmp_twin() {
        let dir = scratch();
        let mut c = cap(8, &dir);
        c.push(b"abcdefghijkl");
        let r = c.finish();
        let final_path = r.spill.expect("final spill path");
        let name = final_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        // 14 hex chars + .txt, at the 2-hex shard level.
        assert_eq!(name.len(), 18, "14 hex + .txt: {name}");
        let shard = final_path.parent().unwrap();
        assert_eq!(shard.file_name().unwrap().len(), 2, "2-hex shard");
        assert_eq!(shard.parent().unwrap(), dir.path().join("spill/bash-exec"));
        let tmps: Vec<PathBuf> = std::fs::read_dir(dir.path().join("spill/bash-exec"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(".tmp-"))
            })
            .collect();
        assert_eq!(tmps.len(), 1, "one tmp twin: {tmps:?}");
        assert_eq!(
            final_path.metadata().unwrap().ino(),
            tmps[0].metadata().unwrap().ino(),
            "hard link: same inode"
        );
        assert_eq!(std::fs::read(&tmps[0]).unwrap(), b"abcdefghijkl");
    }

    /// A pre-occupied content-addressed file with identical bytes is
    /// reused: the second stream lands on the same name and inode without
    /// re-linking.
    #[test]
    fn finalize_reuses_a_matching_preoccupied_file() {
        let dir = scratch();
        let mut first = cap(8, &dir);
        first.push(b"shared stream bytes");
        let path1 = first.finish().spill.expect("first final");
        let ino1 = path1.metadata().unwrap().ino();

        let mut second = BoundedCapture::new(
            8,
            "nonce-2",
            "out",
            dir.path().join("spill"),
            DiskQuota::default(),
        );
        second.push(b"shared stream bytes");
        let path2 = second.finish().spill.expect("second final");
        assert_eq!(path1, path2, "content addressing: same name");
        assert_eq!(
            path2.metadata().unwrap().ino(),
            ino1,
            "reused, not relinked"
        );
    }

    /// A pre-occupied name holding different bytes is a collision: the
    /// capture poisons and surfaces no path, per the never-silently-reuse
    /// contract.
    #[test]
    fn finalize_mismatch_on_preoccupied_name_poisons() {
        let dir = scratch();
        let mut probe = cap(8, &dir);
        probe.push(b"seed bytes");
        let name_path = probe.finish().spill.expect("seed final");
        std::fs::write(&name_path, b"foreign").unwrap();

        let mut c = cap(8, &dir);
        c.push(b"seed bytes");
        let r = c.finish();
        assert!(r.spill.is_none(), "mismatch poisons: no path surfaced");
        assert!(r.text.contains("bytes omitted"));
    }

    /// The task-wide disk cap is shared across both streams (per-task, not
    /// per-spill): two captures on one quota flow up to the combined total
    /// and cap the moment either would exceed it.
    #[test]
    fn two_captures_share_one_quota_and_cap_at_combined_total() {
        let dir = scratch();
        let quota = DiskQuota::new(24);
        let mut out =
            BoundedCapture::new(4, "nonce-a", "out", dir.path().join("spill"), quota.clone());
        let mut err =
            BoundedCapture::new(4, "nonce-b", "err", dir.path().join("spill"), quota.clone());
        // 12 + 12 = exactly the cap: strictly-at-cap still flows.
        assert_eq!(out.push(b"aaaaaaaaaaaa"), PushOutcome::Flowing);
        assert_eq!(err.push(b"bbbbbbbbbbbb"), PushOutcome::Flowing);
        // One byte past the combined total caps both streams.
        assert_eq!(out.push(b"c"), PushOutcome::Capped);
        assert_eq!(err.push(b"c"), PushOutcome::Capped);
        // Spilled bytes stay strictly within the cap: 12 + 12, no more.
        let out_spilled = std::fs::read(out.finish().spill.unwrap()).unwrap();
        let err_spilled = std::fs::read(err.finish().spill.unwrap()).unwrap();
        assert_eq!(out_spilled.len() + err_spilled.len(), 24);
        assert_eq!(out_spilled, b"aaaaaaaaaaaa".as_slice());
        assert_eq!(err_spilled, b"bbbbbbbbbbbb".as_slice());
    }
}
