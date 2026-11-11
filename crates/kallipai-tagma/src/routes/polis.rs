//! The `/polis` surface: the tagma's own relay entries, read and written by the
//! operator who owns this instance.
//!
//! All three faces are operator-only. An entry names a platform origin and the
//! enrolled identity the tagma holds there, which is operator-level
//! configuration; no agent identity reads or changes it. The tagma keeps no
//! owner PAT: the caller brings an enrollment code, and the call spends it
//! (`activate_relay`). The gateway's target list is the set resolved at
//! startup, so every write response says in as many words that this boot's
//! list did not move.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use kallipai_common::protocol::ApiError;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::{AuthIdentity, require_operator};
use crate::polis_config::{self, PolisConfigError};
use crate::state::SharedState;
use crate::{EnrollEntry, RelayEntry};

/// What a write did, and what it did not: the gateway's target list is the
/// boot's, so an added entry serves the relay now and the gateway next start.
const ADDED_NOTE: &str = concat!(
    "relay connected; the gateway's target list is still the one from startup, so ",
    "this entry joins it on the next start"
);

/// The removal half of the same sentence.
const REMOVED_NOTE: &str = concat!(
    "entry removed and its relay stopped; the gateway's target list is still the ",
    "one from startup, so this entry leaves it on the next start"
);

/// The same name and origin, credentials already stored, but nothing running:
/// the relay is what this call changed, and the note says so.
const STARTED_NOTE: &str = concat!(
    "already attached: the entry and its credentials are unchanged; no relay ",
    "was running and one is started now"
);

/// GET /polis — one row per entry: what the file declares, plus the boot's one
/// entry when the configuration came from the environment.
pub(crate) async fn list_polis(
    State(state): State<SharedState>,
    auth: AuthIdentity,
) -> Result<Json<Value>, ApiError> {
    require_operator(auth.identity())?;
    Ok(Json(json!({
        "entries": rows(&state)?,
        "sugar": state.polis_sugar,
    })))
}

/// POST /polis — add one entry, enroll or reuse its credentials, start its
/// relay, then write the file.
pub(crate) async fn add_polis(
    State(state): State<SharedState>,
    auth: AuthIdentity,
    Json(body): Json<AddPolisBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require_operator(auth.identity())?;
    refuse_sugar(&state)?;
    // The file and the relay map are two halves of one state: one writer at
    // a time, so a POST and a DELETE cannot interleave into an entry with no
    // connector (or a connector with no entry).
    let _write = state.polis_writes.lock().await;
    let (name, origin) = polis_config::check_entry(&body.name, &body.url).map_err(config_error)?;
    let entries = polis_config::read_entries().map_err(config_error)?;
    let existing = entries.iter().find(|entry| entry.name == name).cloned();
    let entry_dir = credentials_dir()?.join(&name);
    let stored = crate::credentials::load_tagma(&entry_dir);

    // A code is never spent on a live enrollment: resolve_enroll_entry would
    // ignore it and the platform would keep a registration nobody redeems.
    if stored.is_some() && body.code.is_some() {
        return Err(ApiError::conflict(format!(
            "{name} is already enrolled; re-post without the code to reuse its \
             credentials, or delete it first to rebind"
        )));
    }

    // Rebinding the pinned entry is refused: the pin names an origin, and the
    // boot's stale-pin arm is what explains a server that does not come up.
    if let Some(pinned) =
        crate::settings::load_profile_source_polis().map_err(ApiError::internal)?
        && existing
            .as_ref()
            .is_some_and(|entry| entry.polis_url == pinned && entry.polis_url != origin)
    {
        return Err(ApiError::conflict(format!(
            "{name} is the entry the model profile source is pinned to; switch or \
             clear [profiles.source] polis in settings first"
        )));
    }

    // The code the file carries (a first-run boot's leftover) is spent here
    // when the request brings none, so a configured-but-unenrolled entry can
    // be attached by name alone.
    let code = body.code.or_else(|| {
        existing
            .as_ref()
            .and_then(|entry| entry.enrollment_code.clone())
    });

    let boot = match (stored.is_some(), code) {
        (true, _) => {
            let same_origin = existing
                .as_ref()
                .is_some_and(|entry| entry.polis_url == origin);
            if same_origin && state.relay(&name).is_some() {
                return Ok((
                    StatusCode::OK,
                    Json(json!({
                        "entry": row_from_file(&state, &name)?,
                        "note": "already attached: the entry, its credentials and its relay are unchanged",
                    })),
                ));
            }
            if same_origin {
                attach_existing_relay(&state, &name, &origin).await?;
                return Ok((
                    StatusCode::OK,
                    Json(json!({
                        "entry": row_from_file(&state, &name)?,
                        "note": STARTED_NOTE,
                    })),
                ));
            }
            crate::resolve_enroll_entry(stored.as_ref(), &origin, None)
                .map_err(ApiError::internal)?
        }
        (false, Some(code)) => EnrollEntry::Fresh { code },
        (false, None) => {
            return Err(ApiError::bad_request(format!(
                "{name} is not enrolled and the request carries no code: mint a \
                 platform access token in the web UI, store it with `kallipctl polis \
                 token set <polis>`, then mint an enrollment code against that \
                 registration and send it as \"code\""
            )));
        }
    };

    if let Err(failure) = start_relay(&state, &name, &origin, boot).await {
        return Err(activation_error(
            &name,
            &entry_dir,
            stored.is_some(),
            failure,
        ));
    }

    if let Err(error) = polis_config::add(&name, &origin) {
        // Roll back what this call started, so a refused commit leaves the
        // instance exactly as it was.
        let stopped = stop_relay(&state, &name).await;
        if stored.is_none() {
            let _ = std::fs::remove_dir_all(&entry_dir);
        }
        // A first enrollment leaves a registration on the platform that the
        // rollback cannot undo: the code is spent either way, so the sentence
        // names the way out.
        let platform_note = if stored.is_none() {
            "; the platform registration from this enrollment still exists, \
             remove it with `kallipctl polis revoke <polis> <tagma-id>`"
        } else {
            ""
        };
        return Err(ApiError::internal(format!(
            "the entry could not be written to polis.toml ({error}); the relay was \
             {} and credentials {}{platform_note}",
            describe_stop(stopped),
            if stored.is_none() {
                "created by this call were removed"
            } else {
                "kept (they predate this call)"
            }
        )));
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "entry": row_from_file(&state, &name)?,
            "note": ADDED_NOTE,
        })),
    ))
}

/// DELETE /polis/{name} — drop the entry from the file, stop that relay alone,
/// and forget its credentials only when asked.
pub(crate) async fn remove_polis(
    State(state): State<SharedState>,
    Path(name): Path<String>,
    Query(query): Query<RemovePolisQuery>,
    auth: AuthIdentity,
) -> Result<Json<Value>, ApiError> {
    require_operator(auth.identity())?;
    refuse_sugar(&state)?;
    // The same transaction as `add_polis`: file and relay map move together.
    let _write = state.polis_writes.lock().await;
    let entries = polis_config::read_entries().map_err(config_error)?;
    let Some(entry) = entries.iter().find(|entry| entry.name == name) else {
        return Err(ApiError::not_found(format!(
            "no polis entry named {name:?} in polis.toml"
        )));
    };
    if let Some(pinned) =
        crate::settings::load_profile_source_polis().map_err(ApiError::internal)?
        && entry.polis_url == pinned
    {
        return Err(ApiError::conflict(format!(
            "{name} is the entry the model profile source is pinned to; switch or \
             clear [profiles.source] polis in settings first"
        )));
    }

    polis_config::remove(&name).map_err(config_error)?;
    let stopped = stop_relay(&state, &name).await;
    let credentials = if query.forget {
        match credentials_dir() {
            Ok(root) => match std::fs::remove_dir_all(root.join(&name)) {
                Ok(()) => "deleted",
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => "absent",
                Err(error) => {
                    return Err(ApiError::internal(format!(
                        "{name} is gone from polis.toml and its relay is stopped, \
                         but its credentials directory could not be deleted: {error}"
                    )));
                }
            },
            Err(error) => return Err(ApiError::internal(error)),
        }
    } else {
        "kept"
    };

    Ok(Json(json!({
        "name": name,
        "removed": true,
        "relay": describe_stop(stopped),
        "credentials": credentials,
        "note": REMOVED_NOTE,
    })))
}

#[derive(Deserialize)]
pub(crate) struct AddPolisBody {
    name: String,
    url: String,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct RemovePolisQuery {
    #[serde(default)]
    forget: bool,
}

/// Every entry the tagma knows: the file's, in its order, plus the boot's own
/// entry when that entry is not in the file. Only the sugar boot can produce
/// that second kind: its one entry lives in the environment.
fn rows(state: &SharedState) -> Result<Vec<Value>, ApiError> {
    let entries = polis_config::read_entries().map_err(config_error)?;
    let mut rows = Vec::with_capacity(entries.len() + 1);
    for entry in &entries {
        rows.push(row(state, &entry.name, &entry.polis_url)?);
    }
    // A sugar boot's one entry lives in the environment rather than the file.
    if state.polis_sugar {
        for (name, origin) in state.gateway_faces() {
            if !entries.iter().any(|entry| entry.name == name) {
                rows.push(row(state, &name, &origin)?);
            }
        }
    }
    Ok(rows)
}

/// One row of `GET /polis`: the origin, whether credentials are stored (the
/// shared enrollment predicate), the enrolled id, whether a relay is installed
/// in this process, and whether the boot resolved this name as a gateway face.
fn row(state: &SharedState, name: &str, origin: &str) -> Result<Value, ApiError> {
    let stored = crate::credentials::load_tagma(&credentials_dir()?.join(name));
    Ok(json!({
        "name": name,
        "url": origin,
        "enrolled": stored.is_some(),
        "tagma_id": stored.map(|stored| stored.id),
        "relay": state.relay(name).is_some(),
        "gateway": state.is_gateway_face(name),
    }))
}

/// A row built from what the file holds now, so a response never reports an
/// entry the file does not carry.
fn row_from_file(state: &SharedState, name: &str) -> Result<Value, ApiError> {
    let entries = polis_config::read_entries().map_err(config_error)?;
    let entry = entries
        .iter()
        .find(|entry| entry.name == name)
        .ok_or_else(|| ApiError::internal(format!("{name} is not in polis.toml")))?;
    row(state, &entry.name, &entry.polis_url)
}

/// Start the relay of an entry that already owns its slot: same name and
/// origin on disk, credentials stored, nothing running in this process.
async fn attach_existing_relay(
    state: &SharedState,
    name: &str,
    origin: &str,
) -> Result<(), ApiError> {
    let entry_dir = credentials_dir()?.join(name);
    let stored = crate::credentials::load_tagma(&entry_dir);
    let boot =
        crate::resolve_enroll_entry(stored.as_ref(), origin, None).map_err(ApiError::internal)?;
    start_relay(state, name, origin, boot)
        .await
        .map_err(|failure| activation_error(name, &entry_dir, true, failure))
}

/// Build the relay entry from a name and origin, hand it a shutdown child
/// token, and activate it: the one way both write paths start a relay.
async fn start_relay(
    state: &SharedState,
    name: &str,
    origin: &str,
    boot: EnrollEntry,
) -> Result<(), crate::ActivationFailure> {
    let entry = RelayEntry {
        name: name.to_string(),
        polis_url: origin.to_string(),
        enrollment_code: None,
    };
    let cancel = state.shutdown.child_token();
    crate::activate_relay(state, &entry, boot, cancel).await
}

/// Map an activation failure onto the status its caller can act on. A call
/// that found no credentials creates the entry's directory before the
/// platform answers, so a failure that stored nothing removes it again.
fn activation_error(
    name: &str,
    entry_dir: &std::path::Path,
    preexisting: bool,
    failure: crate::ActivationFailure,
) -> ApiError {
    if !preexisting && crate::credentials::load_tagma(entry_dir).is_none() {
        let _ = std::fs::remove_dir_all(entry_dir);
    }
    match failure {
        crate::ActivationFailure::Refused(error) => ApiError::bad_request(format!(
            "the platform refused the enrollment code: {error:#}"
        )),
        crate::ActivationFailure::Unconfirmed(error) => ApiError::bad_gateway(format!(
            "the platform gave no usable answer: {error:#}; check `kallipctl polis status \
             {name}` for a registration before minting another code"
        )),
        crate::ActivationFailure::CredentialsNotStored { tagma_id, error } => {
            ApiError::internal_with_code(
                format!(
                    "the platform enrolled {name} as {tagma_id}, but its credentials could \
                     not be stored: {error:#}; the registration exists on the platform, so \
                     remove it with `kallipctl polis revoke <polis> {tagma_id}` or retry the \
                     enrollment"
                ),
                "polis_enrollment_not_stored",
            )
        }
        crate::ActivationFailure::Local(error) => ApiError::internal_with_code(
            format!("the relay could not be prepared on this tagma: {error:#}"),
            "polis_activation_failed",
        ),
        crate::ActivationFailure::Connector(error) => ApiError::bad_gateway(format!(
            "credentials stored, but the connector did not start: {error:#}"
        )),
    }
}

/// The data root's credentials directory.
fn credentials_dir() -> Result<std::path::PathBuf, ApiError> {
    crate::credentials_dir().map_err(ApiError::internal)
}

/// A boot configured from the sugar must not write the file: a `polis.toml`
/// beside `KALLIPAI_POLIS_URL` is exactly the conflict the next boot fails
/// fast on, so this face would be able to make the instance unstartable.
fn refuse_sugar(state: &SharedState) -> Result<(), ApiError> {
    if state.polis_sugar {
        return Err(ApiError::conflict(
            "this tagma's polis configuration comes from KALLIPAI_POLIS_URL (or \
             KALLIPAI_TAGMA_RELAY_ENROLLMENT_CODE); a polis.toml beside those \
             variables is the configuration conflict the next start refuses on. \
             Change the deployment unit's environment instead of writing the file \
             here",
        ));
    }
    Ok(())
}

/// Stop one relay: cancel its own token, then await its tunnel task under the
/// same bound the shutdown drain uses. `None` when the entry had no relay.
async fn stop_relay(state: &SharedState, name: &str) -> Option<bool> {
    let relay = state.take_relay(name)?;
    relay.cancel.cancel();
    let bound = std::time::Duration::from_secs(crate::shutdown::GRACEFUL_SHUTDOWN_TIMEOUT_SECS);
    Some(tokio::time::timeout(bound, relay.join).await.is_ok())
}

/// The words a response uses for [`stop_relay`]'s outcome.
fn describe_stop(stopped: Option<bool>) -> &'static str {
    match stopped {
        Some(true) => "stopped",
        Some(false) => "cancelled but its task did not drain in time; it will end with the process",
        None => "not running",
    }
}

/// Map the file layer's verdicts onto status codes: the request's fault is a
/// 400, and the document on disk is a 500 whose message survives: the operator
/// who owns the file can act on it, where a generic "internal error" would read
/// as a server fault to someone in the middle of a maintenance edit.
fn config_error(error: PolisConfigError) -> ApiError {
    match error {
        PolisConfigError::Invalid(message) => ApiError::bad_request(message),
        PolisConfigError::Broken(message) => ApiError::internal_with_code(
            format!("polis.toml could not be read: {message}; fix the file and retry"),
            "polis_file_unreadable",
        ),
        PolisConfigError::NotRemoved(message) => ApiError::internal_with_code(
            format!(
                "polis.toml could not be removed: {message}; the entry is still there and \
                 its relay was left running"
            ),
            "polis_file_not_removed",
        ),
    }
}

#[cfg(test)]
mod tests;
