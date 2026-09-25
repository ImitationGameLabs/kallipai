//! Lesche relay methods for [`TagmaClient`].
//!
//! Deliveries to the user (`kallip lesche send`) and room history reads —
//! the relay resource domain. Split from `client.rs` verbatim; the client
//! core stays in the parent module.

use super::TagmaClient;
use anyhow::{Context, Result};
use kallipai_common::agentid::AgentId;

impl TagmaClient {
    /// Deliver a message via the tagma's relay (`POST
    /// /agents/{id}/lesche/messages`). The agent's `kallip lesche send`
    /// subcommand calls this; the tagma posts an `AssistantContent` envelope.
    /// Returns the tagma's delivery verdict.
    ///
    /// `room` is the optional room id (a reply into a multi-member room);
    /// `tagma` the optional peer tagma id (a send into that tagma's direct
    /// session; create-or-get on first send). Exactly one may be set; both
    /// `None` is the bilateral 1:1 send.
    pub async fn post_message_delivery(
        &self,
        id: &AgentId,
        text: &str,
        room: Option<&str>,
        tagma: Option<&str>,
    ) -> Result<crate::types::LescheMessageResponse> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/agents/{id}/lesche/messages")))
                    .json(&crate::types::LescheMessageRequest {
                        text: text.to_owned(),
                        room: room.map(str::to_owned),
                        tagma: tagma.map(str::to_owned),
                    }),
            )
            .send()
            .await
            .context("failed to send message")?,
            "failed to parse message response",
        )
        .await
    }

    /// List the rooms this tagma has joined (`GET /agents/{id}/lesche/rooms`).
    /// The agent's `kallip lesche rooms` subcommand calls this.
    pub async fn list_joined_rooms(&self, id: &AgentId) -> Result<Vec<String>> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/lesche/rooms"))),
            )
            .send()
            .await
            .context("failed to list rooms")?,
            "failed to parse rooms response",
        )
        .await
    }

    /// Read a room's history (`GET
    /// /agents/{id}/lesche/rooms/{room}/messages`), as a readable text block the
    /// tagma renders server-side. The agent's `kallip lesche read --room`
    /// subcommand calls this. Returns the raw text body (the tagma route renders
    /// one bracketed block per message), NOT JSON.
    pub async fn read_room_messages(
        &self,
        id: &AgentId,
        room: &str,
        after_seq: Option<i64>,
        limit: Option<u64>,
    ) -> Result<String> {
        let mut query = Vec::new();
        if let Some(a) = after_seq {
            query.push(("after_seq", a.to_string()));
        }
        if let Some(l) = limit {
            query.push(("limit", l.to_string()));
        }
        let response = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/lesche/rooms/{room}/messages")))
                    .query(&query),
            )
            .send()
            .await
            .context("failed to read room history")?;
        if !response.status().is_success() {
            return Err(super::error_from_response(response).await);
        }
        response
            .text()
            .await
            .context("failed to read room history body")
    }

    /// Read a direct session's history (`GET
    /// /agents/{id}/lesche/direct-sessions/{peer}/messages`), as a readable
    /// text block the tagma renders server-side. The agent's `kallip lesche
    /// read --tagma <peer>` subcommand calls this; the session id is derived
    /// tagma-side from (self, peer). Returns the raw text body (one bracketed
    /// block per message), NOT JSON.
    pub async fn read_direct_session_messages(
        &self,
        id: &AgentId,
        peer: &str,
        after_seq: Option<i64>,
        limit: Option<u64>,
    ) -> Result<String> {
        let mut query = Vec::new();
        if let Some(a) = after_seq {
            query.push(("after_seq", a.to_string()));
        }
        if let Some(l) = limit {
            query.push(("limit", l.to_string()));
        }
        let response = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!(
                        "/agents/{id}/lesche/direct-sessions/{peer}/messages"
                    )))
                    .query(&query),
            )
            .send()
            .await
            .context("failed to read direct session history")?;
        if !response.status().is_success() {
            return Err(super::error_from_response(response).await);
        }
        response
            .text()
            .await
            .context("failed to read direct session history body")
    }

    /// List every addressable surface (bilateral + rooms + direct sessions,
    /// `GET /agents/{id}/lesche/sessions`). The agent's `kallip lesche
    /// sessions` subcommand calls this.
    pub async fn list_lesche_sessions(
        &self,
        id: &AgentId,
    ) -> Result<Vec<crate::types::LescheSessionEntry>> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/lesche/sessions"))),
            )
            .send()
            .await
            .context("failed to list sessions")?,
            "failed to parse sessions response",
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallipai_common::protocol::ApiError;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> TagmaClient {
        TagmaClient::builder(&server.uri()).build().unwrap()
    }

    fn as_api_error(err: &anyhow::Error) -> &ApiError {
        err.downcast_ref::<ApiError>()
            .expect("downcasts to ApiError")
    }

    #[tokio::test]
    async fn read_room_messages_extracts_envelope_message_from_error_body() {
        let server = MockServer::start().await;
        let id = AgentId::random();
        let room = "room-1";
        Mock::given(method("GET"))
            .and(path(format!("/agents/{id}/lesche/rooms/{room}/messages")))
            .respond_with(
                ResponseTemplate::new(503)
                    .set_body_string(r#"{"error":{"message":"room offline"}}"#),
            )
            .mount(&server)
            .await;
        let err = client_for(&server)
            .read_room_messages(&id, room, None, None)
            .await
            .expect_err("503");
        let api = as_api_error(&err);
        assert_eq!(api.status, 503);
        assert_eq!(api.message, "room offline");
    }

    #[tokio::test]
    async fn read_room_messages_falls_back_to_raw_body_when_not_json() {
        let server = MockServer::start().await;
        let id = AgentId::random();
        let room = "room-1";
        Mock::given(method("GET"))
            .and(path(format!("/agents/{id}/lesche/rooms/{room}/messages")))
            .respond_with(ResponseTemplate::new(500).set_body_string("relay unreachable"))
            .mount(&server)
            .await;
        let err = client_for(&server)
            .read_room_messages(&id, room, None, None)
            .await
            .expect_err("500");
        let api = as_api_error(&err);
        assert_eq!(api.status, 500);
        assert_eq!(api.message, "relay unreachable");
    }

    #[tokio::test]
    async fn read_room_messages_returns_text_body_on_success() {
        let server = MockServer::start().await;
        let id = AgentId::random();
        let room = "room-1";
        Mock::given(method("GET"))
            .and(path(format!("/agents/{id}/lesche/rooms/{room}/messages")))
            .respond_with(ResponseTemplate::new(200).set_body_string("[alice] hello"))
            .mount(&server)
            .await;
        let text = client_for(&server)
            .read_room_messages(&id, room, None, None)
            .await
            .expect("history renders");
        assert_eq!(text, "[alice] hello");
    }

    #[tokio::test]
    async fn read_direct_session_messages_returns_text_body_on_success() {
        let server = MockServer::start().await;
        let id = AgentId::random();
        let peer = "tagma-peer";
        Mock::given(method("GET"))
            .and(path(format!(
                "/agents/{id}/lesche/direct-sessions/{peer}/messages"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string("[peer] hi"))
            .mount(&server)
            .await;
        let text = client_for(&server)
            .read_direct_session_messages(&id, peer, None, None)
            .await
            .expect("history renders");
        assert_eq!(text, "[peer] hi");
    }

    #[tokio::test]
    async fn read_direct_session_messages_extracts_envelope_message_from_error_body() {
        let server = MockServer::start().await;
        let id = AgentId::random();
        let peer = "tagma-peer";
        Mock::given(method("GET"))
            .and(path(format!(
                "/agents/{id}/lesche/direct-sessions/{peer}/messages"
            )))
            .respond_with(
                ResponseTemplate::new(503)
                    .set_body_string(r#"{"error":{"message":"session offline"}}"#),
            )
            .mount(&server)
            .await;
        let err = client_for(&server)
            .read_direct_session_messages(&id, peer, None, None)
            .await
            .expect_err("503");
        let api = as_api_error(&err);
        assert_eq!(api.status, 503);
        assert_eq!(api.message, "session offline");
    }

    #[tokio::test]
    async fn list_lesche_sessions_decodes_bilateral_room_and_direct_entries() {
        let server = MockServer::start().await;
        let id = AgentId::random();
        Mock::given(method("GET"))
            .and(path(format!("/agents/{id}/lesche/sessions")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "kind": "bilateral", "id": "conv-1" },
                { "kind": "room", "id": "room-1", "name": "ops" },
                {
                    "kind": "direct",
                    "id": "sess-1",
                    "peer_tagma": "tagma-peer",
                    "peer_handle": "Peer"
                }
            ])))
            .mount(&server)
            .await;
        let sessions = client_for(&server)
            .list_lesche_sessions(&id)
            .await
            .expect("sessions decode");
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0].kind, "bilateral");
        assert_eq!(sessions[0].name, None);
        assert_eq!(sessions[1].name.as_deref(), Some("ops"));
        assert_eq!(sessions[1].peer_tagma, None);
        assert_eq!(sessions[2].peer_tagma.as_deref(), Some("tagma-peer"));
        assert_eq!(sessions[2].peer_handle.as_deref(), Some("Peer"));
    }

    #[tokio::test]
    async fn post_message_delivery_sends_tagma_field_for_direct_send() {
        let server = MockServer::start().await;
        let id = AgentId::random();
        use wiremock::matchers::body_partial_json;
        Mock::given(method("POST"))
            .and(path(format!("/agents/{id}/lesche/messages")))
            .and(body_partial_json(serde_json::json!({
                "text": "ping",
                "tagma": "tagma-peer"
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })),
            )
            .mount(&server)
            .await;
        let verdict = client_for(&server)
            .post_message_delivery(&id, "ping", None, Some("tagma-peer"))
            .await
            .expect("delivery accepted");
        assert!(verdict.ok);
    }
}
