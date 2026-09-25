//! Usage extraction from upstream responses.
//!
//! The forwarding path streams the upstream body to the client verbatim;
//! this module's scanners ride along as pure spectators: they never hold or
//! transform the forwarded bytes, they only look for the OpenAI usage block
//! so the audit row can carry token counts.
//!
//! Two shapes:
//!
//! - [`usage_from_json`]: a buffered (non-streaming) response body.
//! - [`SseUsageScanner`]: an incremental scanner for `text/event-stream`
//!   bodies -- fed chunk by chunk as they are forwarded, tolerant of lines
//!   split across chunks, bounded memory (the residual-line buffer is
//!   capped; a non-conforming stream only loses its usage, never its
//!   forwarding).
//!
//! Streaming usage appears only when the client asked for it
//! (`stream_options: {"include_usage": true}` puts a usage object on the
//! final SSE chunk); without it the token counts stay NULL and the audit
//! row still lands. The last usage object seen wins.

use serde_json::Value;

/// Token counts extracted from an OpenAI `usage` block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtractedUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

/// Cap for the residual (not yet newline-terminated) line buffer. A real
/// SSE data line is one chat chunk -- kilobytes; the cap is pure defense
/// against a non-conforming stream.
const RESIDUAL_LINE_CAP: usize = 1024 * 1024;

/// Dig the usage block out of a parsed OpenAI chunk/body value.
fn usage_from_value(value: &Value) -> Option<ExtractedUsage> {
    let usage = value.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    Some(ExtractedUsage {
        prompt_tokens: usage.get("prompt_tokens")?.as_i64()?,
        completion_tokens: usage.get("completion_tokens")?.as_i64()?,
        total_tokens: usage.get("total_tokens")?.as_i64()?,
    })
}

/// Extract usage from a buffered response body (the non-streaming shape).
pub fn usage_from_json(body: &[u8]) -> Option<ExtractedUsage> {
    usage_from_value(&serde_json::from_slice(body).ok()?)
}

/// Incremental usage scanner for SSE streams (see the module doc).
#[derive(Debug, Default)]
pub struct SseUsageScanner {
    buf: Vec<u8>,
    found: Option<ExtractedUsage>,
}

impl SseUsageScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one forwarded chunk. Bytes are inspected, never retained beyond
    /// a partial trailing line.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        // Whole lines are consumed first, so the cap below only ever sees
        // the unterminated trailing partial line; a lone partial line with
        // no newline carries nothing parseable, and clearing it is safe.
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            if let Some(usage) = sse_line_usage(&line) {
                self.found = Some(usage); // the last usage block wins
            }
        }
        if self.buf.len() > RESIDUAL_LINE_CAP {
            self.buf.clear();
        }
    }

    /// Flush the trailing partial line (call once after the stream ends).
    pub fn finish(mut self) -> Option<ExtractedUsage> {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            if let Some(usage) = sse_line_usage(&line) {
                self.found = Some(usage);
            }
        }
        self.found
    }
}

/// One complete SSE line: strip the `data:` frame, parse the JSON, dig out
/// the usage block. Everything else (comments, `[DONE]`, event frames,
/// non-JSON) is a cheap no.
fn sse_line_usage(line: &[u8]) -> Option<ExtractedUsage> {
    let payload = line.trim_ascii().strip_prefix(b"data:")?;
    let payload = payload.trim_ascii();
    if payload.is_empty() || payload == b"[DONE]" {
        return None;
    }
    usage_from_value(&serde_json::from_slice(payload).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USAGE: &str = r#"{"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}"#;

    fn usage_json_line(model_choice: bool) -> String {
        let choice = if model_choice {
            r#""choices":[{"message":{"role":"assistant"}}],"#
        } else {
            r#""choices":[],"#
        };
        format!(r#"{{"id":"cmpl-1",{choice}"usage":{USAGE}}}"#)
    }

    #[test]
    fn usage_from_json_reads_a_plain_body() {
        let body = usage_json_line(true);
        let usage = usage_from_json(body.as_bytes()).expect("usage present");
        assert_eq!(usage.prompt_tokens, 11);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 18);
    }

    #[test]
    fn usage_from_json_misses_a_body_without_usage() {
        let body = br#"{"id":"cmpl-1","choices":[]}"#;
        assert!(usage_from_json(body).is_none());
    }

    #[test]
    fn scanner_reads_a_usage_line_and_skips_the_rest() {
        let mut scanner = SseUsageScanner::new();
        let stream = format!(
            "data: {}\n\n:data comment\n[data noise]\ndata: [DONE]\nnot json at all\n",
            usage_json_line(false)
        );
        scanner.feed(stream.as_bytes());
        let usage = scanner.finish().expect("usage extracted");
        assert_eq!(usage.total_tokens, 18);
    }

    #[test]
    fn scanner_tolerates_lines_split_across_chunks() {
        let line = format!("data: {}\n\n", usage_json_line(false));
        let bytes = line.as_bytes();
        let mut scanner = SseUsageScanner::new();
        // Split at an awkward point: inside the "data: " prefix and mid-JSON.
        for split in [2, 9, 20, bytes.len() / 2, bytes.len() - 3] {
            scanner.feed(&bytes[..split]);
            scanner.feed(&bytes[split..]);
        }
        let usage = scanner.finish().expect("usage survives chunk tearing");
        assert_eq!(usage.total_tokens, 18);
    }

    #[test]
    fn scanner_last_usage_wins() {
        let mut scanner = SseUsageScanner::new();
        let first = format!("data: {}\n\n", usage_json_line(false));
        let last = format!("data: {}\n\n", usage_json_line(true));
        scanner.feed(first.as_bytes());
        scanner.feed(last.as_bytes());
        let usage = scanner.finish().expect("usage extracted");
        assert_eq!(usage.prompt_tokens, 11);
    }

    #[test]
    fn scanner_flushes_a_trailing_partial_line() {
        let mut scanner = SseUsageScanner::new();
        // No trailing newline: the usage line sits in the residual buffer.
        scanner.feed(format!("data: {}", usage_json_line(false)).as_bytes());
        let usage = scanner.finish().expect("partial line flushed");
        assert_eq!(usage.total_tokens, 18);
    }

    #[test]
    fn scanner_no_usage_anywhere_finishes_none() {
        let mut scanner = SseUsageScanner::new();
        scanner.feed(b"data: {\"choices\":[]}\n\ndata: [DONE]\n\n");
        assert!(scanner.finish().is_none());
    }

    #[test]
    fn scanner_huge_lineless_chunk_stays_bounded() {
        let mut scanner = SseUsageScanner::new();
        let blob = vec![b'x'; 3 * RESIDUAL_LINE_CAP];
        scanner.feed(&blob);
        // The cap path clears the buffer; nothing panics, nothing retained.
        assert!(scanner.finish().is_none());
    }

    #[test]
    fn scanner_whole_lines_in_an_oversized_chunk_are_still_scanned() {
        let mut scanner = SseUsageScanner::new();
        // One oversized chunk whose tail carries a complete usage data line:
        // the residual cap must not discard it.
        let mut blob = vec![b'x'; RESIDUAL_LINE_CAP + 64];
        blob.push(b'\n');
        blob.extend_from_slice(b"data: ");
        blob.extend_from_slice(usage_json_line(false).as_bytes());
        blob.push(b'\n');
        scanner.feed(&blob);
        let usage = scanner.finish().expect("usage inside the chunk survives");
        assert_eq!(usage.total_tokens, 18);
    }
}
