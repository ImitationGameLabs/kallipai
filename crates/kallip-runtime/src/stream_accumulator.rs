//! Streaming tool-call accumulator.
//!
//! Accumulates tool-call deltas (which arrive in chunks across multiple SSE
//! events, indexed by position) into complete `ChatToolCall` objects.

use std::collections::BTreeMap;

use just_llm_client::types::chat::{
    ChatCompletionChunkToolCall, ChatToolCall, FunctionCall, ToolType,
};

pub(super) struct AccumulatedToolCall {
    id: Option<String>,
    kind: Option<ToolType>,
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

    pub(super) fn push(&mut self, delta: &ChatCompletionChunkToolCall) {
        let index = delta.index.unwrap_or(0);
        let entry = self.calls.entry(index).or_insert(AccumulatedToolCall {
            id: None,
            kind: None,
            name: None,
            arguments: String::new(),
        });
        if let Some(id) = &delta.id {
            entry.id = Some(id.clone());
        }
        if let Some(kind) = &delta.kind {
            entry.kind = Some(kind.clone());
        }
        if let Some(func) = &delta.function {
            if let Some(name) = &func.name {
                entry.name = Some(name.clone());
            }
            if let Some(args) = &func.arguments {
                entry.arguments.push_str(args);
            }
        }
    }

    pub(super) fn finish(self) -> Vec<ChatToolCall> {
        self.calls
            .into_values()
            .map(|acc| ChatToolCall {
                id: acc.id.unwrap_or_default(),
                kind: acc.kind.unwrap_or(ToolType::Function),
                function: FunctionCall {
                    name: acc.name.unwrap_or_default(),
                    arguments: acc.arguments,
                },
            })
            .collect()
    }
}
