//! Task-domain client methods for [`TagmaClient`].
//!
//! Mirrors the task routes served by `kallip-tagma` under `/tasks`.

use super::TagmaClient;
use anyhow::{Context, Result};
use kallip_common::protocol::{
    TaskCloseRequest, TaskConfirmRequest, TaskCreateRequest, TaskExport, TaskForceRequest,
    TaskListPage, TaskListQuery, TaskNoteRequest,
};

impl TagmaClient {
    pub async fn task_create(&self, req: &TaskCreateRequest) -> Result<TaskExport> {
        self.handle_response(
            self.with_auth(self.inner.http.post(self.url("/tasks")).json(&req))
                .send()
                .await
                .context("create task")?,
            "parse task export",
        )
        .await
    }

    pub async fn task_start(&self, id: i64, req: &TaskForceRequest) -> Result<TaskExport> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/tasks/{id}/start")))
                    .json(&req),
            )
            .send()
            .await
            .context("start task")?,
            "parse task export",
        )
        .await
    }

    pub async fn task_list(&self, query: &TaskListQuery) -> Result<TaskListPage> {
        self.handle_response(
            self.with_auth(self.inner.http.get(self.url("/tasks")).query(query))
                .send()
                .await
                .context("list tasks")?,
            "parse task list",
        )
        .await
    }
    pub async fn task_show(&self, id: i64) -> Result<TaskExport> {
        self.handle_response(
            self.with_auth(self.inner.http.get(self.url(&format!("/tasks/{id}"))))
                .send()
                .await
                .context("show task")?,
            "parse task export",
        )
        .await
    }

    pub async fn task_export_all(&self) -> Result<Vec<TaskExport>> {
        self.handle_response(
            self.with_auth(self.inner.http.get(self.url("/tasks/export")))
                .send()
                .await
                .context("export tasks")?,
            "parse task list",
        )
        .await
    }

    pub async fn task_confirm(&self, id: i64, req: &TaskConfirmRequest) -> Result<TaskExport> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/tasks/{id}/confirm")))
                    .json(&req),
            )
            .send()
            .await
            .context("confirm task")?,
            "parse task export",
        )
        .await
    }

    pub async fn task_review(&self, id: i64) -> Result<TaskExport> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/tasks/{id}/review"))),
            )
            .send()
            .await
            .context("review task")?,
            "parse task export",
        )
        .await
    }

    pub async fn task_pause(&self, id: i64) -> Result<TaskExport> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/tasks/{id}/pause"))),
            )
            .send()
            .await
            .context("pause task")?,
            "parse task export",
        )
        .await
    }

    pub async fn task_resume(&self, id: i64, req: &TaskForceRequest) -> Result<TaskExport> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/tasks/{id}/resume")))
                    .json(&req),
            )
            .send()
            .await
            .context("resume task")?,
            "parse task export",
        )
        .await
    }

    pub async fn task_note(&self, id: i64, req: &TaskNoteRequest) -> Result<TaskExport> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/tasks/{id}/note")))
                    .json(&req),
            )
            .send()
            .await
            .context("note task")?,
            "parse task export",
        )
        .await
    }

    pub async fn task_close(&self, id: i64, req: &TaskCloseRequest) -> Result<TaskExport> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/tasks/{id}/close")))
                    .json(&req),
            )
            .send()
            .await
            .context("close task")?,
            "parse task export",
        )
        .await
    }

    pub async fn task_reopen(&self, id: i64, req: &TaskForceRequest) -> Result<TaskExport> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/tasks/{id}/reopen")))
                    .json(&req),
            )
            .send()
            .await
            .context("reopen task")?,
            "parse task export",
        )
        .await
    }

    pub async fn task_archive(&self, id: i64, req: &TaskForceRequest) -> Result<TaskExport> {
        self.handle_response(
            self.with_auth(
                self.inner
                    .http
                    .post(self.url(&format!("/tasks/{id}/archive")))
                    .json(&req),
            )
            .send()
            .await
            .context("archive task")?,
            "parse task export",
        )
        .await
    }
    /// Downloads the closed-task archive blob (canonical tar bytes).
    pub async fn task_fetch_archive(&self, id: i64) -> Result<Vec<u8>> {
        let resp = self
            .with_auth(
                self.inner
                    .http
                    .get(self.url(&format!("/tasks/{id}/archive"))),
            )
            .send()
            .await
            .context("fetch task archive")?;
        let resp = self.ensure_success(resp).await?;
        Ok(resp.bytes().await?.to_vec())
    }
}
