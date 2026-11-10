//! The loop's behaviour tests, one file per question it answers: what a send does
//! (`sends`), which exit a failure takes (`exits`), and what the episode's ledger
//! and clock allow (`accounting`).

pub(crate) use std::sync::Arc;
pub(crate) use std::time::Duration;

pub(crate) use futures_util::StreamExt;
pub(crate) use just_llm_client::types::generation::GenerationEvent;
pub(crate) use just_llm_client::{
    BackendError, GenerationClientOptions, LlmBackend, TransportError,
    provider::OpenAiCompatBackend,
};
pub(crate) use kallipai_common::facts::{Block, BlockKind};
pub(crate) use kallipai_common::retry::RetryKind;
pub(crate) use tokio_util::sync::CancellationToken;
pub(crate) use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

pub(crate) use crate::profile::GenerationClient;
pub(super) use crate::retry::test_support::{
    fast_policy, mock_client, mount_status, mount_status_with_body, retrying_count, run_turn,
};
pub(crate) use crate::retry::{
    Episode, RequestFailure, RetryCall, RetryPolicy, max_sends, stream_with_retry,
};
pub(crate) use crate::test_support::{
    QUOTA_RESET_BODY, RecordingBackend, RecordingPacing, request, user_msg,
};
pub(crate) use kallipai_common::timefmt;

mod accounting;
mod exits;
mod sends;
