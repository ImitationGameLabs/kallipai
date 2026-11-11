//! The caller's own tagmata as the registry lists them: `GET /tagmata`.
//!
//! Shared by the route and the clients that read it, so the field names a
//! caller decodes cannot drift from the ones the archeion emits.

use crate::ids::TagmaId;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// One row of the owner's tagma list (pending or enrolled, never revoked).
///
/// Liveness ("is a tunnel open for this tagma right now") is deliberately not
/// part of this shape: the keryx reports `TagmaOnline`/`TagmaOffline` on its
/// own stream, and `tagmata.last_tunnel_proof_ts` is a replay guard rather
/// than a liveness signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagmaSummary {
    pub tagma_id: TagmaId,
    pub label: Option<String>,
    pub state: TagmaState,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Pending-phase display mask (`sk-enroll-abc***xyz`). Present only while
    /// pending; omitted for enrolled rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_masked: Option<String>,
    /// Pending-phase code expiry. Present only while pending; omitted for
    /// enrolled rows.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub expires_at: Option<OffsetDateTime>,
}

/// The lifecycle state the list exposes: a code waiting to be used, or a
/// registered tagma. Revoked rows are not listed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagmaState {
    Pending,
    Enrolled,
    /// A state this client predates. The list is the place an operator
    /// looks when something is wrong, so one unrecognized word must not
    /// fail the whole answer: the row still prints, and the state prints
    /// as `unknown`.
    #[serde(other)]
    Unknown,
}

impl TagmaState {
    /// The word this state prints as, the same one the wire uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Enrolled => "enrolled",
            Self::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn pending() -> TagmaSummary {
        TagmaSummary {
            tagma_id: TagmaId::from("tagma-1".to_owned()),
            label: Some("laptop".to_owned()),
            state: TagmaState::Pending,
            created_at: datetime!(2026-01-01 00:00:00 UTC),
            code_masked: Some("sk-enroll-abc***xyz".to_owned()),
            expires_at: Some(datetime!(2026-01-01 01:00:00 UTC)),
        }
    }

    #[test]
    fn the_wire_names_are_the_ones_the_list_route_emits() {
        let json = serde_json::to_value(pending()).expect("serialize");
        assert_eq!(json["tagma_id"], "tagma-1");
        assert_eq!(json["state"], "pending");
        assert_eq!(json["created_at"], "2026-01-01T00:00:00Z");
        assert_eq!(json["code_masked"], "sk-enroll-abc***xyz");
        assert_eq!(json["expires_at"], "2026-01-01T01:00:00Z");
    }

    #[test]
    fn an_enrolled_row_omits_the_pending_fields() {
        let enrolled = TagmaSummary {
            state: TagmaState::Enrolled,
            code_masked: None,
            expires_at: None,
            ..pending()
        };
        let json = serde_json::to_value(&enrolled).expect("serialize");
        assert_eq!(json["state"], "enrolled");
        assert!(json.get("code_masked").is_none());
        assert!(json.get("expires_at").is_none());
        let back: TagmaSummary = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, enrolled);
        assert_eq!(back.state.as_str(), "enrolled");
    }

    #[test]
    fn an_unknown_state_still_decodes_a_row() {
        let mut json = serde_json::to_value(pending()).expect("serialize");
        json["state"] = serde_json::json!("draining");
        let back: TagmaSummary = serde_json::from_value(json).expect("an unknown word decodes");
        assert_eq!(back.state, TagmaState::Unknown);
        assert_eq!(back.state.as_str(), "unknown");
        assert_eq!(back.tagma_id.as_ref(), "tagma-1");
    }
}
