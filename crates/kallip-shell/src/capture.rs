//! Bounded output capture for one stdout/stderr stream.
//!
//! Keeps a rolling in-memory tail (the last `max_bytes`), so a runaway command
//! can't exhaust memory. The `truncated` flag tells the caller the output was
//! clipped; a future enhancement spills the full stream to a file and returns
//! its path (as Codex / Claude Code do) so the agent can `Read` it.

/// A bounded byte collector for one stream.
#[derive(Default)]
pub(super) struct BoundedCapture {
    max_bytes: usize,
    tail: Vec<u8>,
    total: usize,
}

/// The finalized capture of one stream.
#[derive(Debug, Default, Clone)]
pub(super) struct CaptureResult {
    /// The (possibly clipped) output, lossily decoded as UTF-8.
    pub text: String,
    /// `true` if `total` bytes exceeded `max_bytes` (the head was dropped).
    pub truncated: bool,
}

impl BoundedCapture {
    /// Creates a collector that retains at most `max_bytes` of recent output.
    pub(super) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            tail: Vec::new(),
            total: 0,
        }
    }

    /// Append a chunk, trimming the in-memory tail once over budget.
    pub(super) fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > self.max_bytes {
            // Keep only the most recent `max_bytes` (the tail the LLM sees).
            let start = self.tail.len() - self.max_bytes;
            self.tail.drain(0..start);
        }
    }

    /// Finalize into a [`CaptureResult`].
    pub(super) fn finish(self) -> CaptureResult {
        CaptureResult {
            text: String::from_utf8_lossy(&self.tail).into_owned(),
            truncated: self.total > self.max_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_budget_keeps_everything() {
        let mut c = BoundedCapture::new(10);
        c.push(b"hello");
        let r = c.finish();
        assert_eq!(r.text, "hello");
        assert!(!r.truncated);
    }

    #[test]
    fn over_budget_keeps_tail_and_flags_truncated() {
        let mut c = BoundedCapture::new(5);
        c.push(b"abcdefghij"); // 10 bytes, budget 5
        let r = c.finish();
        assert_eq!(r.text, "fghij"); // last 5
        assert!(r.truncated);
    }

    #[test]
    fn chunks_then_overflow() {
        let mut c = BoundedCapture::new(4);
        c.push(b"ab");
        c.push(b"cd");
        c.push(b"ef");
        let r = c.finish();
        assert_eq!(r.text, "cdef");
        assert!(r.truncated);
    }
}
