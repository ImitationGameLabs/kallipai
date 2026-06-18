//! Line-oriented ring buffer for accumulating PTY output.

/// Ring buffer that accumulates PTY output line by line.
///
/// When the number of lines exceeds `max_lines`, the oldest lines are
/// discarded automatically.
pub(super) struct ScrollbackBuffer {
    lines: Vec<String>,
    max_lines: usize,
}

impl ScrollbackBuffer {
    pub(super) fn new(max_lines: usize) -> Self {
        Self {
            lines: Vec::with_capacity(1024),
            max_lines,
        }
    }

    pub(super) fn append_line(&mut self, line: &str) {
        self.lines.push(line.to_owned());
        if self.lines.len() > self.max_lines {
            let excess = self.lines.len() - self.max_lines;
            self.lines.drain(..excess);
        }
    }

    /// Returns the last `n` lines joined with `\n`.
    pub(super) fn last_n(&self, n: usize) -> String {
        let start = self.lines.len().saturating_sub(n);
        self.lines[start..].join("\n")
    }

    /// Returns the full buffer joined with `\n`.
    pub(super) fn snapshot(&self) -> String {
        self.lines.join("\n")
    }
}
