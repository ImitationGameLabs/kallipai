//! Handler-level tests for the provider vault: create in both modes, list,
//! full replacement (metadata edit, key rotation, mode flip), delete, the
//! per-account cap, the duplicate-name 409, cross-account isolation, and the
//! mode whitelist. The handlers are called directly with a seeded
//! `Principal::User` (mirrors `routes/emails/tests.rs`).

use super::{ProviderRequest, create_provider, delete_provider, list_providers, replace_provider};
use crate::auth::{AuthPrincipal, Principal};
use crate::test_helpers::{make_state, seed_user};
use axum::Json;
use axum::extract::State;
use time::OffsetDateTime;

fn req(name: &str, provider: &str, key: &str, mode: &str) -> Json<ProviderRequest> {
    Json(ProviderRequest {
        name: name.to_string(),
        provider: provider.to_string(),
        base_url: None,
        key_material: key.to_string(),
        mode: mode.to_string(),
    })
}

#[tokio::test]
async fn create_stores_both_modes_verbatim() {
    let state = make_state().await;
    let user = seed_user(&state, "alice").await;

    // Plaintext row: the raw key round-trips as stored.
    let Json(sum) = create_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user.clone())),
        req("work key", "anthropic", "sk-ant-plain", "plaintext"),
    )
    .await
    .expect("plaintext create ok");
    assert_eq!(sum.mode, "plaintext");
    assert_eq!(sum.key_material, "sk-ant-plain");

    // Encrypted row: the blob round-trips verbatim -- the server never
    // interprets key_material in either mode.
    let Json(sum) = create_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user.clone())),
        req("encrypted key", "openai", "blob:v1:deadbeef", "encrypted"),
    )
    .await
    .expect("encrypted create ok");
    assert_eq!(sum.mode, "encrypted");
    assert_eq!(sum.key_material, "blob:v1:deadbeef");

    let Json(list) = list_providers(State(state), AuthPrincipal(Principal::User(user)))
        .await
        .expect("list ok");
    assert_eq!(list.len(), 2);
    // Oldest first (created_at ascending).
    assert_eq!(list[0].name, "work key");
    assert_eq!(list[1].name, "encrypted key");
}

#[tokio::test]
async fn replace_rotates_key_and_flips_mode() {
    let state = make_state().await;
    let user = seed_user(&state, "alice").await;

    let Json(created) = create_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user.clone())),
        req("main", "anthropic", "sk-ant-1", "plaintext"),
    )
    .await
    .expect("create ok");

    // Key rotation: same mode, new key material.
    let Json(rotated) = replace_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user.clone())),
        axum::extract::Path(created.id),
        req("main", "anthropic", "sk-ant-2", "plaintext"),
    )
    .await
    .expect("rotate ok");
    assert_eq!(rotated.key_material, "sk-ant-2");
    assert_eq!(rotated.created_at, created.created_at);

    // Mode flip: the client submits the newly encrypted blob; the server
    // swaps the stored form without interpreting either side.
    let Json(flipped) = replace_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user.clone())),
        axum::extract::Path(created.id),
        req("main", "anthropic", "blob:v1:cafebabe", "encrypted"),
    )
    .await
    .expect("flip ok");
    assert_eq!(flipped.mode, "encrypted");
    assert_eq!(flipped.key_material, "blob:v1:cafebabe");
}

#[tokio::test]
async fn rename_onto_sibling_name_conflicts() {
    let state = make_state().await;
    let user = seed_user(&state, "alice").await;

    let Json(_) = create_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user.clone())),
        req("work", "anthropic", "sk-1", "plaintext"),
    )
    .await
    .expect("create a");
    let Json(b) = create_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user.clone())),
        req("main", "anthropic", "sk-2", "plaintext"),
    )
    .await
    .expect("create b");

    // Renaming b onto a's name trips the UNIQUE (user_id, name) index
    // through the UPDATE path (insert has its own test above).
    let err = replace_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user.clone())),
        axum::extract::Path(b.id),
        req("work", "openai", "sk-3", "plaintext"),
    )
    .await
    .expect_err("rename conflict 409");
    assert_eq!(err.status, 409);

    // The conflicted write left both rows untouched.
    let Json(list) = list_providers(State(state), AuthPrincipal(Principal::User(user)))
        .await
        .expect("list ok");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].key_material, "sk-1");
    assert_eq!(list[1].key_material, "sk-2");
}

#[tokio::test]
async fn delete_removes_and_returns_remaining() {
    let state = make_state().await;
    let user = seed_user(&state, "alice").await;

    let Json(a) = create_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user.clone())),
        req("keep", "anthropic", "sk-1", "plaintext"),
    )
    .await
    .expect("create a");
    let Json(_b) = create_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user.clone())),
        req("drop", "openai", "sk-2", "plaintext"),
    )
    .await
    .expect("create b");

    let Json(remaining) = delete_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user.clone())),
        axum::extract::Path(a.id),
    )
    .await
    .expect("delete ok");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].name, "drop");

    // Deleting again: the row is gone (404, not a silent second delete).
    let err = delete_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user.clone())),
        axum::extract::Path(a.id),
    )
    .await
    .expect_err("second delete 404");
    assert_eq!(err.status, 404);
}

#[tokio::test]
async fn duplicate_name_conflicts_within_account() {
    let state = make_state().await;
    let user = seed_user(&state, "alice").await;
    let principal = AuthPrincipal(Principal::User(user));

    let _ = create_provider(
        State(state.clone()),
        principal.clone(),
        req("work", "anthropic", "sk-1", "plaintext"),
    )
    .await
    .expect("first ok");

    // Same account, same name -> 409 (unique per account, not global).
    let err = create_provider(
        State(state.clone()),
        principal.clone(),
        req("work", "openai", "sk-2", "plaintext"),
    )
    .await
    .expect_err("duplicate 409");
    assert_eq!(err.status, 409);

    // Another account may use the same name freely.
    let other = seed_user(&state, "bob").await;
    let _ = create_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(other)),
        req("work", "anthropic", "sk-3", "plaintext"),
    )
    .await
    .expect("cross-account same name ok");
}

#[tokio::test]
async fn cross_account_access_is_404() {
    let state = make_state().await;
    let alice = seed_user(&state, "alice").await;
    let bob = seed_user(&state, "bob").await;

    let Json(created) = create_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(alice.clone())),
        req("mine", "anthropic", "sk-1", "plaintext"),
    )
    .await
    .expect("create ok");

    // Bob cannot read, rewrite, or delete Alice's entry: a foreign id is a
    // 404 either way (no existence leak, mirroring the emails surface).
    for err in [
        replace_provider(
            State(state.clone()),
            AuthPrincipal(Principal::User(bob.clone())),
            axum::extract::Path(created.id),
            req("hijack", "openai", "sk-x", "plaintext"),
        )
        .await
        .expect_err("replace foreign 404"),
        delete_provider(
            State(state.clone()),
            AuthPrincipal(Principal::User(bob)),
            axum::extract::Path(created.id),
        )
        .await
        .expect_err("delete foreign 404"),
    ] {
        assert_eq!(err.status, 404);
    }

    // Alice still sees her row untouched.
    let Json(list) = list_providers(State(state), AuthPrincipal(Principal::User(alice)))
        .await
        .expect("alice list ok");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].key_material, "sk-1");
}

#[tokio::test]
async fn cap_rejects_the_eleventh_entry() {
    let state = make_state().await;
    let user = seed_user(&state, "alice").await;
    let principal = AuthPrincipal(Principal::User(user));

    for i in 0..10 {
        let _ = create_provider(
            State(state.clone()),
            principal.clone(),
            req(&format!("p{i}"), "anthropic", "sk", "plaintext"),
        )
        .await
        .expect("within cap");
    }

    let err = create_provider(
        State(state.clone()),
        principal,
        req("p10", "anthropic", "sk", "plaintext"),
    )
    .await
    .expect_err("cap 409");
    assert_eq!(err.status, 409);
}

#[tokio::test]
async fn unknown_mode_is_rejected() {
    let state = make_state().await;
    let user = seed_user(&state, "alice").await;

    let err = create_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user)),
        req("x", "anthropic", "sk", "hashed"),
    )
    .await
    .expect_err("bad mode 400");
    assert_eq!(err.status, 400);
}

// Wire-format guard: the summary must serialize its timestamps as RFC3339.
// time's default serde for OffsetDateTime is the space-separated Display
// form ("2026-08-26 23:50:00.123 +00:00:00"), which JS Date cannot parse --
// seen live as "Added Invalid Date" in the vault UI before the fix.
#[tokio::test]
async fn summary_timestamps_serialize_as_rfc3339() {
    let state = make_state().await;
    let user = seed_user(&state, "carol").await;
    let Json(sum) = create_provider(
        State(state.clone()),
        AuthPrincipal(Principal::User(user)),
        req("ts key", "anthropic", "sk-ant-ts", "plaintext"),
    )
    .await
    .expect("create ok");

    let json = serde_json::to_value(&sum).expect("serialize summary");
    for field in ["created_at", "updated_at"] {
        let raw = json[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} not a string"));
        let parsed = OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|e| panic!("{field} is not RFC3339 ({raw}): {e}"));
        assert_eq!(parsed.unix_timestamp(), sum.created_at.unix_timestamp());
    }
}
