//! Streaming tool-call accumulator.
//!
//! Accumulates tool-call deltas (which arrive fragmented across multiple SSE
//! events, identified by a stable in-flight index) into complete
//! [`ToolCall`] objects.
//!
//! Why index-keyed: protocols stream a tool call's `id`/`name` on the first
//! fragment and `arguments` as appended JSON fragments afterwards; parallel
//! calls interleave, so fragments are grouped by [`ToolCallDelta::index`] (0
//! when the protocol omits it — the single-call common case).

use std::collections::BTreeMap;

use just_llm_client::types::generation::{ToolCall, ToolCallDelta};

struct AccumulatedToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

pub(super) struct ToolCallAccumulator {
    calls: BTreeMap<u32, AccumulatedToolCall>,
}

impl ToolCallAccumulator {
    pub(super) fn new() -> Self {
        Self {
            calls: BTreeMap::new(),
        }
    }

    pub(super) fn push(&mut self, delta: &ToolCallDelta) {
        let index = delta.index.unwrap_or(0);
        let entry = self.calls.entry(index).or_insert(AccumulatedToolCall {
            id: None,
            name: None,
            arguments: String::new(),
        });
        if let Some(id) = &delta.id {
            entry.id = Some(id.clone());
        }
        if let Some(name) = &delta.name {
            entry.name = Some(name.clone());
        }
        if let Some(args) = &delta.arguments {
            entry.arguments.push_str(args);
        }
    }

    pub(super) fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_values()
            .map(|acc| ToolCall {
                id: acc.id.unwrap_or_default(),
                name: acc.name.unwrap_or_default(),
                arguments: acc.arguments,
            })
            .collect()
    }
}
