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

use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;
use std::path::PathBuf;

use nix::fcntl::{OFlag, open, openat};
use nix::sys::stat::Mode;

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
    /// Per-exec uuid nonce; names the spill file so concurrent execs and the two
    /// streams of one exec never collide.
    nonce: String,
    /// Stream label embedded in the spill filename: `merged`, `stdout`, or
    /// `stderr` (the backend chooses per capture mode).
    stream_label: &'static str,
    /// Where the spill file is created (a landlocked-readable temp dir).
    spill_dir: PathBuf,
}

/// Spill-file lifecycle for [`BoundedCapture::spill`].
#[derive(Default)]
enum SpillState {
    /// Not yet overflowing; no file created.
    #[default]
    Closed,
    /// Overflowing; the file is open and being appended.
    Open(SpillHandle),
    /// Spill failed irrecoverably; degrade to head+tail view with no path.
    Poisoned,
}

/// An open spill file and its path.
struct SpillHandle {
    file: File,
    path: PathBuf,
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
}

impl BoundedCapture {
    /// Creates a collector that retains a head of `max_bytes/2` and a tail of
    /// the remainder, spilling the full stream to `spill_dir` on overflow.
    pub(super) fn new(
        max_bytes: usize,
        nonce: &str,
        stream_label: &'static str,
        spill_dir: PathBuf,
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
        }
    }

    /// Append a chunk: lazily open the spill on the first overflow, append to it
    /// thereafter, and feed the in-memory head (until frozen) then the rolling
    /// tail. The spill-flush ordering is load-bearing: the head+tail prefix is
    /// flushed *before* the overflowing chunk is appended, so the file ends up
    /// byte-identical to the true stream.
    pub(super) fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        let will_overflow = self.total > self.max_bytes;

        // Lazy spill: open on the FIRST overflow, flushing the in-memory view
        // (head + tail = the complete prefix so far, <= max_bytes) so the file
        // ultimately holds the entire stream.
        if will_overflow && matches!(self.spill, SpillState::Closed) {
            match self.open_spill_with_head_and_tail() {
                Ok(handle) => self.spill = SpillState::Open(handle),
                Err(_) => self.spill = SpillState::Poisoned,
            }
        }
        // Every chunk after the spill opens is appended, so the file is complete.
        if let SpillState::Open(handle) = &mut self.spill
            && handle.file.write_all(chunk).is_err()
        {
            // Mid-stream write failure: stop spilling and keep what we have;
            // surface no path so the caller never points at a partial file.
            self.spill = SpillState::Poisoned;
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
    }

    /// Create the spill file, writing the current head+tail prefix first. The
    /// caller then appends each subsequent chunk via the `Open` arm of `push`.
    ///
    /// TOCTOU-safe against a symlink swapped in at the spill path between build
    /// and this first overflow: the dir is opened with `O_NOFOLLOW | O_DIRECTORY`
    /// (refuses a symlink at the final component, pins the real dir inode), then
    /// the file is created with `openat` relative to that dirfd and `O_NOFOLLOW`
    /// at the leaf. Done back-to-back at overflow time, there is no check/use
    /// window for the leaf: once the dirfd is held, swapping the path for a
    /// symlink cannot redirect the `openat` (it is relative to the inode). The
    /// dir is created here, lazily, so under-budget captures write nothing.
    ///
    /// Scope note: `O_NOFOLLOW` only guards the final component, so a symlink on
    /// an *intermediate* component of `spill_dir` (a parent) is still followed.
    /// Acceptable because `spill_dir` is daemon-controlled and defaults to
    /// `temp_dir()/kallip` (real parents); closing it would need `openat2` with
    /// `RESOLVE_NO_SYMLINKS`, which is Linux-5.6+ only and this file stays
    /// portable-Unix (no `cfg` gates).
    fn open_spill_with_head_and_tail(&self) -> std::io::Result<SpillHandle> {
        // Best-effort: create the dir tree only when absent. `create_dir_all`
        // follows symlinks, but a symlink at the leaf of `spill_dir` is refused
        // by the `O_NOFOLLOW` open below, so this only ever creates real dirs.
        let _ = std::fs::create_dir_all(&self.spill_dir);
        let dirfd = open(
            &self.spill_dir,
            OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_RDONLY,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let filename = format!("bash_exec-{}-{}.txt", self.nonce, self.stream_label);
        let file = openat(
            dirfd.as_fd(),
            filename.as_str(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(std::io::Error::from)?;
        let mut file = File::from(file);
        file.write_all(&self.head)?;
        file.write_all(&self.tail)?;
        Ok(SpillHandle {
            file,
            path: self.spill_dir.join(filename),
        })
    }

    /// Finalize into a [`CaptureResult`], rendering the head+tail view (with a
    /// middle-omitted marker on overflow) and surfacing the spill path when the
    /// spill is healthy. The spill `File` is dropped here, closing the fd;
    /// writes already reached the page cache, so a same-host reader sees them.
    pub(super) fn finish(mut self) -> CaptureResult {
        let spill = match std::mem::replace(&mut self.spill, SpillState::Closed) {
            SpillState::Open(handle) => Some(handle.path),
            _ => None,
        };
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
        CaptureResult {
            text,
            truncated,
            spill,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch dir that isolates spill files to the test and cleans them on drop.
    fn scratch() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    fn cap(budget: usize, dir: &tempfile::TempDir) -> BoundedCapture {
        BoundedCapture::new(budget, "nonce", "out", dir.path().to_path_buf())
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
            PathBuf::from("/nonexistent-kallip-test-dir-xyz"),
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
}
