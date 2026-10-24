//! Lesche relay methods for [`TagmaClient`].
//!
//! Deliveries to the user (`kallip lesche send`) and room history reads —
//! the relay resource domain. Split from `client.rs` verbatim; the client
//! core stays in the parent module.

use super::TagmaClient;
use anyhow::{Context, Result};
use kallipai_common::agentid::AgentId;
use kallipai_lesche_common::ids::path_segment;

/// The page a room-history read asks for. The two positions mean opposite
/// directions, so they travel as named fields: a page moves one way at a time
/// (the data plane rejects a request naming both), and a caller that leaves
/// `before_seq` at `None` sends no such key at all -- `0` there reads as "no
/// row", not as "from the start". The data plane's own `RoomHistoryQuery`
/// carries the same three fields; the two are kept in step by hand, because
/// this crate does not depend on `kallipai-lesche-client`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RoomReadQuery {
    /// Rows with `seq > after_seq` (forward: the first page of newer rows).
    pub after_seq: Option<i64>,
    /// Rows with `seq < before_seq` (backward: the newest page of older rows,
    /// returned ascending).
    pub before_seq: Option<i64>,
    /// Page size; the data plane clamps it.
    pub limit: Option<u64>,
}

/// The page a direct-session read asks for. The direct log is read forward
/// only (`seq > after_seq`), so there is no backward cursor here: the page
/// ending below a position does not exist on this surface. A single-row
/// lookup -- the row a `| quote=<seq>` segment names -- is that same forward
/// cursor one step below the position, with a page of one.
#[derive(Debug, Default, Clone, Copy)]
pub struct DirectReadQuery {
    /// Rows with `seq > after_seq` (forward: the first page of newer rows).
    pub after_seq: Option<i64>,
    /// Page size; the data plane caps it, and floors nothing.
    pub limit: Option<u64>,
}

/// A delivery's target and reference. `room` and `tagma` are two same-typed
/// options, so they travel as named fields: exactly one may be set (both
/// `None` is the bilateral 1:1 send), and swapping them would post the message
/// to the wrong surface. `quote` names a row in the room's log or in the
/// direct session's own, and `mentions` names members of that room.
#[derive(Debug, Default, Clone, Copy)]
pub struct MessageDelivery<'a> {
    /// The room to post into (a reply into a multi-member room).
    pub room: Option<&'a str>,
    /// The peer tagma whose direct session receives the message.
    pub tagma: Option<&'a str>,
    /// The position, in that conversation's log, that this message answers.
    pub quote: Option<i64>,
    /// The participant ids this message names, raw as the caller wrote them:
    /// shape, the cap and the roster are the tagma's to judge. Empty is the
    /// same as none -- the request leaves the key off the wire.
    pub mentions: &'a [String],
}

impl TagmaClient {
    /// Deliver a message via the tagma's relay (`POST
    /// /agents/{id}/lesche/messages`). The agent's `kallip lesche send`
    /// subcommand calls this; the tagma posts an `AssistantContent` envelope.
    /// Returns the tagma's delivery verdict.
    ///
    /// [`MessageDelivery`] carries the target -- a room, or a peer tagma's
    /// direct session, created on first send -- and the position this message
    /// answers.
    pub async fn post_message_delivery(
        &self,
        id: &AgentId,
        text: &str,
        delivery: MessageDelivery<'_>,
    ) -> Result<crate::types::LescheMessageResponse> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!(
                        "/agents/{}/lesche/messages",
                        path_segment(id.as_ref())?
                    )))
                    .json(&crate::types::LescheMessageRequest {
                        text: text.to_owned(),
                        room: delivery.room.map(str::to_owned),
                        tagma: delivery.tagma.map(str::to_owned),
                        quote: delivery.quote,
                        mentions: delivery.mentions.to_vec(),
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
            self.with_auth(self.inner.http.get(self.url(&format!(
                "/agents/{}/lesche/rooms",
                path_segment(id.as_ref())?
            ))))
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
        query: RoomReadQuery,
    ) -> Result<String> {
        let mut params = Vec::new();
        if let Some(a) = query.after_seq {
            params.push(("after_seq", a.to_string()));
        }
        if let Some(b) = query.before_seq {
            params.push(("before_seq", b.to_string()));
        }
        if let Some(l) = query.limit {
            params.push(("limit", l.to_string()));
        }
        let response = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!(
                        "/agents/{id}/lesche/rooms/{}/messages",
                        path_segment(room)?
                    )))
                    .query(&params),
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
    ///
    /// A [`DirectReadQuery`] that names one row -- `after_seq` one below a
    /// `quote` position, `limit` one -- is the lookup behind a
    /// `| quote=<seq>` segment.
    pub async fn read_direct_session_messages(
        &self,
        id: &AgentId,
        peer: &str,
        query: DirectReadQuery,
    ) -> Result<String> {
        let mut params = Vec::new();
        if let Some(a) = query.after_seq {
            params.push(("after_seq", a.to_string()));
        }
        if let Some(l) = query.limit {
            params.push(("limit", l.to_string()));
        }
        let response = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!(
                        "/agents/{id}/lesche/direct-sessions/{}/messages",
                        path_segment(peer)?
                    )))
                    .query(&params),
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

    /// Read a direct session's history addressed by the session id itself
    /// (`GET /agents/{id}/lesche/sessions/{session}/messages`).
    ///
    /// The agent's `kallip lesche read --session <session>` subcommand calls
    /// this. The id is the one an inbound header already spells (`direct
    /// <session>`), so no peer pair is derived here: the command printed in a
    /// delivered message names the log it came from, verbatim. Returns the raw
    /// text body (one bracketed block per message), NOT JSON.
    pub async fn read_direct_session_by_id_messages(
        &self,
        id: &AgentId,
        session: &str,
        query: DirectReadQuery,
    ) -> Result<String> {
        let mut params = Vec::new();
        if let Some(a) = query.after_seq {
            params.push(("after_seq", a.to_string()));
        }
        if let Some(l) = query.limit {
            params.push(("limit", l.to_string()));
        }
        let response = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!(
                        "/agents/{id}/lesche/sessions/{}/messages",
                        path_segment(session)?
                    )))
                    .query(&params),
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

    /// Read the bilateral 1:1 row one id names (`GET
    /// /agents/{id}/external/history/{row}`).
    ///
    /// The agent's `kallip lesche read --quote <id>` subcommand calls this: a
    /// bare read names the 1:1 conversation, and the tagma renders that row as
    /// the same bracketed block the other surfaces print. A row the history does
    /// not hold is the route's error, never an empty body.
    pub async fn read_quote(&self, id: &AgentId, quote: i64) -> Result<String> {
        let response = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{id}/external/history/{quote}"))),
            )
            .send()
            .await
            .context("failed to read the quoted row")?;
        if !response.status().is_success() {
            return Err(super::error_from_response(response).await);
        }
        response
            .text()
            .await
            .context("failed to read the quoted row body")
    }

    /// List every addressable surface (bilateral + rooms + direct sessions,
    /// `GET /agents/{id}/lesche/sessions`). The agent's `kallip lesche
    /// sessions` subcommand calls this.
    pub async fn list_lesche_sessions(
        &self,
        id: &AgentId,
    ) -> Result<Vec<crate::types::LescheSessionEntry>> {
        self.handle_response(
            self.with_auth(self.inner.http.get(self.url(&format!(
                "/agents/{}/lesche/sessions",
                path_segment(id.as_ref())?
            ))))
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
    use wiremock::matchers::{
        body_partial_json, method, path, query_param, query_param_is_missing,
    };
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
            .read_room_messages(&id, room, RoomReadQuery::default())
            .await
            .expect_err("503");
        let api = as_api_error(&err);
        assert_eq!(api.status, 503);
        assert_eq!(api.message, "room offline");
    }

    /// A caller-supplied id rides one path segment: a value carrying separators
    /// or a dot segment cannot reach another route, and arrives as the id it is
    /// rather than reshaped into a different one.
    #[tokio::test]
    async fn a_caller_supplied_id_stays_inside_its_segment() {
        let server = MockServer::start().await;
        let id = AgentId::random();
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;
        let client = client_for(&server);

        client
            .read_room_messages(&id, "../../sessions/abc", RoomReadQuery::default())
            .await
            .expect("the room read is sent");
        client
            .read_direct_session_by_id_messages(&id, "../rooms/abc", DirectReadQuery::default())
            .await
            .expect("the session read is sent");

        let paths = server
            .received_requests()
            .await
            .expect("requests recorded")
            .iter()
            .map(|request| request.url.path().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                format!("/agents/{id}/lesche/rooms/%2E%2E%2F%2E%2E%2Fsessions%2Fabc/messages"),
                format!("/agents/{id}/lesche/sessions/%2E%2E%2Frooms%2Fabc/messages"),
            ],
            "each id stays in its own segment"
        );
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
            .read_room_messages(&id, room, RoomReadQuery::default())
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
            .read_room_messages(&id, room, RoomReadQuery::default())
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
            .read_direct_session_messages(&id, peer, DirectReadQuery::default())
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
            .read_direct_session_messages(&id, peer, DirectReadQuery::default())
            .await
            .expect_err("503");
        let api = as_api_error(&err);
        assert_eq!(api.status, 503);
        assert_eq!(api.message, "session offline");
    }

    #[tokio::test]
    async fn read_direct_session_messages_sends_the_single_row_shape() {
        // The shape a `--quote <seq>` read becomes on the wire: the direct log
        // is read forward only, so the cursor sits one below the position and
        // the page is one row. Both keys are asserted together, so a lost
        // offset or a dropped page size fails here.
        let server = MockServer::start().await;
        let id = AgentId::random();
        let peer = "tagma-peer";
        Mock::given(method("GET"))
            .and(path(format!(
                "/agents/{id}/lesche/direct-sessions/{peer}/messages"
            )))
            .and(query_param("after_seq", "6"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[seq=7] hi"))
            .mount(&server)
            .await;
        let text = client_for(&server)
            .read_direct_session_messages(
                &id,
                peer,
                DirectReadQuery {
                    after_seq: Some(6),
                    limit: Some(1),
                },
            )
            .await
            .expect("single row");
        assert_eq!(text, "[seq=7] hi");
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
            .post_message_delivery(
                &id,
                "ping",
                MessageDelivery {
                    tagma: Some("tagma-peer"),
                    ..MessageDelivery::default()
                },
            )
            .await
            .expect("delivery accepted");
        assert!(verdict.ok);
    }

    #[tokio::test]
    async fn read_room_messages_pages_back_with_before_seq() {
        let server = MockServer::start().await;
        let id = AgentId::random();
        let room = "room-1";
        Mock::given(method("GET"))
            .and(path(format!("/agents/{id}/lesche/rooms/{room}/messages")))
            .and(query_param("before_seq", "5"))
            .and(query_param_is_missing("after_seq"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[seq=4] hi"))
            .mount(&server)
            .await;
        let text = client_for(&server)
            .read_room_messages(
                &id,
                room,
                RoomReadQuery {
                    after_seq: None,
                    before_seq: Some(5),
                    limit: None,
                },
            )
            .await
            .expect("backward page");
        assert_eq!(text, "[seq=4] hi");
    }

    #[tokio::test]
    async fn read_room_messages_sends_no_cursor_that_was_left_unset() {
        // The two cursors are not symmetric: `after_seq = 0` reads from the
        // start while `before_seq = 0` reads as "no row at all", so an unset
        // backward cursor must not travel -- the key is absent, not zero.
        let server = MockServer::start().await;
        let id = AgentId::random();
        let room = "room-1";
        Mock::given(method("GET"))
            .and(path(format!("/agents/{id}/lesche/rooms/{room}/messages")))
            .and(query_param_is_missing("before_seq"))
            .and(query_param_is_missing("after_seq"))
            .and(query_param_is_missing("limit"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[seq=1] hi"))
            .mount(&server)
            .await;
        client_for(&server)
            .read_room_messages(&id, room, RoomReadQuery::default())
            .await
            .expect("plain page");
    }

    #[tokio::test]
    async fn read_room_messages_sends_the_forward_cursor() {
        let server = MockServer::start().await;
        let id = AgentId::random();
        let room = "room-1";
        Mock::given(method("GET"))
            .and(path(format!("/agents/{id}/lesche/rooms/{room}/messages")))
            .and(query_param("after_seq", "3"))
            .and(query_param_is_missing("before_seq"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[seq=4] hi"))
            .mount(&server)
            .await;
        client_for(&server)
            .read_room_messages(
                &id,
                room,
                RoomReadQuery {
                    after_seq: Some(3),
                    before_seq: None,
                    limit: None,
                },
            )
            .await
            .expect("forward page");
    }

    #[tokio::test]
    async fn read_room_messages_sends_the_single_row_shape() {
        // The shape a `--quote <seq>` read becomes on the wire: one row, with
        // the cursor already moved one position up. Both keys are asserted
        // together, so a lost `+1` or a dropped page size fails here.
        let server = MockServer::start().await;
        let id = AgentId::random();
        let room = "room-1";
        Mock::given(method("GET"))
            .and(path(format!("/agents/{id}/lesche/rooms/{room}/messages")))
            .and(query_param("before_seq", "8"))
            .and(query_param("limit", "1"))
            .and(query_param_is_missing("after_seq"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[seq=7] hi"))
            .mount(&server)
            .await;
        let text = client_for(&server)
            .read_room_messages(
                &id,
                room,
                RoomReadQuery {
                    after_seq: None,
                    before_seq: Some(8),
                    limit: Some(1),
                },
            )
            .await
            .expect("single row");
        assert_eq!(text, "[seq=7] hi");
    }

    #[tokio::test]
    async fn post_message_delivery_sends_the_quote() {
        let server = MockServer::start().await;
        let id = AgentId::random();
        Mock::given(method("POST"))
            .and(path(format!("/agents/{id}/lesche/messages")))
            .and(body_partial_json(serde_json::json!({
                "text": "ping",
                "room": "ops",
                "quote": 7
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })),
            )
            .mount(&server)
            .await;
        client_for(&server)
            .post_message_delivery(
                &id,
                "ping",
                MessageDelivery {
                    room: Some("ops"),
                    quote: Some(7),
                    ..MessageDelivery::default()
                },
            )
            .await
            .expect("delivery accepted");
    }

    #[tokio::test]
    async fn post_message_delivery_carries_the_mentions_of_a_room_send() {
        let server = MockServer::start().await;
        let id = AgentId::random();
        use wiremock::matchers::body_partial_json;
        Mock::given(method("POST"))
            .and(path(format!("/agents/{id}/lesche/messages")))
            .and(body_partial_json(serde_json::json!({
                "text": "ping",
                "room": "ops",
                "mentions": ["62626262-6262-4262-8262-626262626262"]
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })),
            )
            .mount(&server)
            .await;
        let mentions = ["62626262-6262-4262-8262-626262626262".to_owned()];
        client_for(&server)
            .post_message_delivery(
                &id,
                "ping",
                MessageDelivery {
                    room: Some("ops"),
                    mentions: mentions.as_slice(),
                    ..MessageDelivery::default()
                },
            )
            .await
            .expect("delivery accepted");
    }

    #[test]
    fn a_message_request_carries_the_mentions_it_was_given() {
        let wire = serde_json::to_value(crate::types::LescheMessageRequest {
            text: "ping".to_owned(),
            room: Some("ops".to_owned()),
            tagma: None,
            quote: None,
            mentions: vec!["62626262-6262-4262-8262-626262626262".to_owned()],
        })
        .expect("serializes");
        assert_eq!(
            wire["mentions"],
            serde_json::json!(["62626262-6262-4262-8262-626262626262"])
        );
    }

    #[test]
    fn a_message_request_omits_an_absent_quote() {
        let wire = serde_json::to_value(crate::types::LescheMessageRequest {
            text: "ping".to_owned(),
            room: Some("ops".to_owned()),
            tagma: None,
            quote: None,
            mentions: Vec::new(),
        })
        .expect("serializes");
        assert!(wire.get("quote").is_none(), "{wire}");
        assert!(wire.get("mentions").is_none(), "{wire}");
        assert!(wire.get("tagma").is_none(), "{wire}");
    }
}
