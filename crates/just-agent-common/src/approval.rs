//! Approval status and tool call content types.
//!
//! Shared between the runtime approval module and the daemon API.

use serde::{Deserialize, Serialize};

/// Status of an approval request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Committed,
    Approved,
    Denied,
    Redeemed,
    Cancelled,
}

impl ApprovalStatus {
    /// Parse a status string (e.g. from a query parameter).
    pub fn from_str_name(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "committed" => Some(Self::Committed),
            "approved" => Some(Self::Approved),
            "denied" => Some(Self::Denied),
            "redeemed" => Some(Self::Redeemed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Committed => "committed",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Redeemed => "redeemed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for ApprovalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Complete tool call content for an approval.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallContent {
    pub tool_name: String,
    pub arguments: serde_json::Value,
}
