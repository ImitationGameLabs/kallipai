//! Team domain methods for [`TagmaClient`]: the declarative team
//! management read face (three-way status) and write face (converge).
//!
//! Thin transport only — planning, preflight, and execution live on the
//! tagma; the CLI folds its lock file into the request and renders the
//! response.

use super::TagmaClient;
use anyhow::{Context, Result};
use kallipai_common::protocol::{
    TeamConvergeOutcome, TeamConvergeRequest, TeamConvergeResponse, TeamStatusQuery,
    TeamStatusResponse,
};

impl TagmaClient {
    /// Fetch the three-way team comparison (declaration vs lock vs live
    /// registry) for one declaration file. Read-only.
    pub async fn team_status(&self, query: &TeamStatusQuery) -> Result<TeamStatusResponse> {
        self.handle_response(
            self.with_auth(self.inner.http.get(self.url("/team/status")).query(&query))
                .send()
                .await
                .context("failed to connect to tagma")?,
            "failed to parse team status response",
        )
        .await
    }

    /// Run converge against one declaration file. The tagma plans,
    /// preflights, and (unless `dry_run`) executes; the response carries
    /// the plan, per-action results, and the lock mapping to write.
    pub async fn team_converge(
        &self,
        req: &TeamConvergeRequest,
    ) -> Result<(TeamConvergeOutcome, TeamConvergeResponse)> {
        let resp: TeamConvergeResponse = self
            .handle_response(
                self.with_auth(self.inner.http.post(self.url("/team/converge")).json(req))
                    .send()
                    .await
                    .context("failed to connect to tagma")?,
                "failed to parse converge response",
            )
            .await?;
        let outcome = resp.outcome;
        Ok((outcome, resp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallipai_common::protocol::{
        ApiError, RoleDisposition, TeamAction, TeamConvergeOutcome, TeamPlanRow,
    };
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> TagmaClient {
        TagmaClient::builder(&server.uri()).build().unwrap()
    }

    #[tokio::test]
    async fn team_status_sends_query_and_parses_rows() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/team/status"))
            .and(query_param("file", "/team/tagma.toml"))
            .and(query_param(
                "lock",
                "dev:00000000-0000-4000-8000-000000000001",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "declaration_path": "/team/tagma.toml",
                "roles": [
                    {
                        "role": "dev",
                        "lock_id": "00000000-0000-4000-8000-000000000001",
                        "lock_drift": false,
                        "lock_inactive": false,
                        "root_conflict": false,
                        "live": [],
                        "disposition": "spawn",
                    },
                ],
            })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let query = TeamStatusQuery {
            file: "/team/tagma.toml".to_string(),
            lock: Some("dev:00000000-0000-4000-8000-000000000001".to_string()),
        };
        let status = client.team_status(&query).await.expect("status parses");
        assert_eq!(status.roles.len(), 1);
        assert_eq!(status.roles[0].disposition, RoleDisposition::Spawn);
    }

    #[tokio::test]
    async fn team_converge_posts_body_and_parses_outcome() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/team/converge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "declaration_path": "/team/tagma.toml",
                "dry_run": true,
                "outcome": "planned",
                "plan": [
                    {
                        "role": "dev",
                        "action": "spawn",
                        "disposition": "spawn",
                        "notes": [],
                    },
                ],
                "results": [],
                "rejections": [],
                "lock": [],
            })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let req = TeamConvergeRequest {
            file: "/team/tagma.toml".to_string(),
            lock: None,
            dry_run: true,
            force: false,
        };
        let (outcome, resp) = client.team_converge(&req).await.expect("converge parses");
        assert_eq!(outcome, TeamConvergeOutcome::Planned);
        let plan: Vec<&TeamPlanRow> = resp.plan.iter().collect();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].action, TeamAction::Spawn);
    }

    #[tokio::test]
    async fn team_converge_propagates_the_mutex_409() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/team/converge"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "error": {"message": "converge already in progress"},
            })))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let req = TeamConvergeRequest {
            file: "/team/tagma.toml".to_string(),
            lock: None,
            dry_run: false,
            force: false,
        };
        let err = client
            .team_converge(&req)
            .await
            .expect_err("409 surfaces as an error");
        let api = err
            .downcast_ref::<ApiError>()
            .expect("downcasts to ApiError");
        assert_eq!(api.status, 409);
        assert_eq!(api.message, "converge already in progress");
    }
}
