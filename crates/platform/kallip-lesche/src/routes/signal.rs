//! Tagma signal relay fan: owner-stream rebroadcast of runtime signals.
//!
//! The batched `POST /tagmata/{tagma_id}/upstream` channel demultiplexes
//! Signal elements into [`relay_signal`], which rebroadcasts each event as
//! a [`LescheEvent::TagmaSignal`] on the owner's app event stream. Like
//! status and presence, the signal is plaintext and user-scoped, so the
//! lesche can read it — these are operator metadata, not conversation
//! content (authored content rides the encrypted envelope). The relay does
//! not interpret the event and does not rate-limit; the tagma still logs
//! each signal to its own application log for observability.
//!
//! Signals are not persisted in `chat_history` and not replayed: a reconnect
//! only replays authored messages. If the owner has no live app stream the
//! signal is silently dropped (best-effort), still 202.
//!
//! Concurrency: routing runs under a registry READ lock (broadcast `send` is
//! synchronous), never co-held with a `ControlPlane` call.

use axum::http::StatusCode;
use kallip_archeion_common::ids::TagmaId;
use kallip_common::protocol::ApiError;
use kallip_common::protocol::SignalEvent;
use kallip_lesche_common::event::LescheEvent;
use tracing::debug;

use crate::state::SharedConvState;

/// The batched upstream channel's signal fan: owner-stream rebroadcast of
/// one runtime signal. Single shared implementation so wire paths cannot
/// drift.
pub(super) async fn relay_signal(
    state: &SharedConvState,
    tagma_id: TagmaId,
    event: SignalEvent,
) -> Result<StatusCode, ApiError> {
    let app_stream = {
        let reg = state.read()?;
        let owner = reg
            .presence_by_tagma(&tagma_id)
            .ok_or_else(|| ApiError::not_found("no live tunnel for tagma"))?
            .owner
            .clone();
        reg.app_stream(&owner)
    };

    if let Some(stream) = app_stream
        && stream
            .deliver(LescheEvent::TagmaSignal {
                tagma_id: tagma_id.clone(),
                event,
            })
            .is_ok()
    {
        debug!(tagma = %tagma_id, "signal relayed");
    }
    Ok(StatusCode::ACCEPTED)
}
