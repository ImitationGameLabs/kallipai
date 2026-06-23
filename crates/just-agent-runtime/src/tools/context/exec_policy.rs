use std::sync::{Arc, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use just_agent_common::policy::ExecPolicy;
use just_llm_client::tools::LlmTool;
use serde_json::{Value, json};

use crate::policy::classifier;

/// Reports the agent's effective `bash_exec` command policy: the per-command
/// overrides layered on the static read-only catalog, plus the structural shell
/// rules. Read-only — the agent cannot change its own exec policy.
pub struct ExecPolicyTool {
    exec_policy: Arc<RwLock<ExecPolicy>>,
}

impl ExecPolicyTool {
    /// Tool name exposed to the LLM and referenced by the policy layer.
    pub const NAME: &str = "exec_policy";

    pub fn new(exec_policy: Arc<RwLock<ExecPolicy>>) -> Self {
        Self { exec_policy }
    }
}

#[async_trait]
impl LlmTool for ExecPolicyTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Report this agent's effective bash_exec command policy: the per-command \
         overrides layered on the static read-only catalog, the catalog itself, \
         and the structural shell rules (composition, background, redirects, etc.). \
         Read-only — you cannot change your own exec policy; a supervisor sets it. \
         Use this to understand which commands run freely, which ask for approval, \
         and which are denied before you call bash_exec."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn call(&self, _args_json: &str) -> Result<String> {
        let policy = self
            .exec_policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let result = json!({
            "overrides": policy.overrides,
            "read_only_catalog": classifier::default_catalog_summary(),
            "structural_rules": classifier::STRUCTURAL_RULES
                .iter()
                .map(|(rule, effect)| json!({ "rule": rule, "effect": effect }))
                .collect::<Vec<_>>(),
        });
        Ok(serde_json::to_string(&result)?)
    }
}
