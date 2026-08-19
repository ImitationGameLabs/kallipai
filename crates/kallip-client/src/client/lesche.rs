//! Lesche relay methods for [`TagmaClient`].
//!
//! Deliveries to the user (`kallip lesche send`) and room history reads —
//! the relay resource domain. Split from `client.rs` verbatim; the client
//! core stays in the parent module.

use super::TagmaClient;
use anyhow::{Context, Result};
use kallip_common::agentid::AgentId;

impl TagmaClient {
    /// Deliver a message to the user via the tagma's relay (`POST
    /// /agents/{id}/lesche/messages`). The agent's `kallip lesche send`
    /// subcommand calls this; the tagma posts an `AssistantContent` envelope.
    /// Returns the tagma's delivery verdict.
    ///
    /// `room` is the optional room id (a reply into a multi-member room);
    /// `None` is the bilateral 1:1 send (no room target).
    pub async fn post_message_delivery(
        &self,
        id: &AgentId,
        text: &str,
        room: Option<&str>,
    ) -> Result<crate::types::LescheMessageResponse> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/agents/{id}/lesche/messages")))
                    .json(&crate::types::LescheMessageRequest {
                        text: text.to_owned(),
                        room: room.map(str::to_owned),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallip_common::protocol::ApiError;
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
}
