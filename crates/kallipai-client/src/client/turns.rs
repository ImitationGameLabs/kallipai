//! Turn-history client methods for [`TagmaClient`].
//!
//! Mirrors the read routes served by `kallipai-tagma` under
//! `/agents/{id}/turns`: one page of the log, one turn, and one call's text.
//! Every answer is computed by the tagma from the log it already keeps; the
//! client never reads a history file itself, because a reader that parses the
//! store turns the store's format into a contract.

use super::TagmaClient;
use anyhow::{Context, Result};
use kallipai_common::agentid::AgentId;
use kallipai_common::protocol::{CallBody, CallBodyQuery, TurnDetail, TurnsPage, TurnsQuery};

impl TagmaClient {
    /// One page of an agent's turns, newest first.
    pub async fn turns_page(&self, agent: &AgentId, query: &TurnsQuery) -> Result<TurnsPage> {
        let document = self.turns_page_document(agent, query).await?;
        serde_json::from_str(&document).context("parse turn page")
    }

    /// One turn, its message text and call rows included.
    pub async fn turn(&self, agent: &AgentId, turn_id: u64) -> Result<TurnDetail> {
        let document = self.turn_document(agent, turn_id).await?;
        serde_json::from_str(&document).context("parse turn")
    }

    /// A byte range of one call's text, as the log holds it.
    pub async fn call_body(
        &self,
        agent: &AgentId,
        turn_id: u64,
        call_index: u32,
        query: &CallBodyQuery,
    ) -> Result<CallBody> {
        let document = self
            .call_body_document(agent, turn_id, call_index, query)
            .await?;
        serde_json::from_str(&document).context("parse call body")
    }

    /// The page as the tagma sent it, bytes and all. A reader that has to
    /// audit a newer view keeps the fields this build does not know, which a
    /// round trip through the typed view would drop.
    pub async fn turns_page_document(&self, agent: &AgentId, query: &TurnsQuery) -> Result<String> {
        let response = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{agent}/turns")))
                    .query(query),
            )
            .send()
            .await
            .context("list turns")?;
        let response = self.ensure_success(response).await?;
        response.text().await.context("read turn page")
    }

    /// One turn as the tagma sent it, for the same reason.
    pub async fn turn_document(&self, agent: &AgentId, turn_id: u64) -> Result<String> {
        let response = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/agents/{agent}/turns/{turn_id}"))),
            )
            .send()
            .await
            .context("read turn")?;
        let response = self.ensure_success(response).await?;
        response.text().await.context("read turn document")
    }

    /// One call's text range as the tagma sent it.
    pub async fn call_body_document(
        &self,
        agent: &AgentId,
        turn_id: u64,
        call_index: u32,
        query: &CallBodyQuery,
    ) -> Result<String> {
        let response = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!(
                        "/agents/{agent}/turns/{turn_id}/calls/{call_index}/body"
                    )))
                    .query(query),
            )
            .send()
            .await
            .context("read call body")?;
        let response = self.ensure_success(response).await?;
        response.text().await.context("read call body document")
    }
}
