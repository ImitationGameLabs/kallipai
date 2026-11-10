//! The message guard, asked as the guard asks it: what an oversized message
//! becomes, and what the ordinary one stays.

use super::super::message_guard::{
    cap_external_message, message_spill_serial, set_message_spill_root,
};

/// The message-entry guard cuts an oversized external message to a
/// banner-topped slice and spills the full original; the spill equality
/// pins the banner's promise — the path the agent is told to Read really
/// holds the untouched original.
#[tokio::test]
async fn message_guard_cuts_oversized_and_spills_original() {
    let _serial = message_spill_serial().await;
    let spill_dir = tempfile::TempDir::new().unwrap();
    set_message_spill_root(Some(spill_dir.path().join("spill")));
    let ctx = crate::test_support::make_ctx(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
    )
    .await;
    // 40K CJK chars ≈ 40K estimated tokens: over the full cap.
    let original = "错".repeat(40_000);
    ctx.record_message_turn(&original).await;

    let store = ctx.store.lock().await;
    let turn = store.turns().back().unwrap();
    let content = turn.messages[0].content().unwrap_or_default();
    assert!(
        content.contains("message truncated"),
        "oversized message must be cut with a banner"
    );
    assert!(content.contains("Full original saved at"));
    assert!(content.contains("resend the content in smaller parts"));
    assert!(
        content.contains("temporary file"),
        "banner must not promise long-term readability"
    );
    assert!(content.chars().count() < original.chars().count());
    // The banner's numbers describe the real keep (same pin style as the
    // tool-result cap banner).
    let banner = &content[content.find("message truncated: kept first").unwrap()..];
    let nums: Vec<usize> = banner
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    assert_eq!(
        nums[2],
        original.chars().count(),
        "banner total must be the message size"
    );
    assert!(nums[0] > nums[1], "head must keep more than the tail");
    assert!(
        nums[0] + nums[1] < nums[2],
        "kept ends must not cover the whole original"
    );
    let spill_path = content
        .split("Full original saved at ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    assert_eq!(std::fs::read_to_string(spill_path).unwrap(), original);
    set_message_spill_root(None);
}

/// The banner's inbox pointer follows the inbox_backed flag: an
/// oversized message cut through an inbox-backed entrance points at
/// the inbox copy, and the same cut through the prompt-channel drain
/// does not — both branches spill and cut identically otherwise.
#[tokio::test]
async fn cut_banner_inbox_pointer_follows_inbox_backed_flag() {
    let _serial = message_spill_serial().await;
    let spill_dir = tempfile::TempDir::new().unwrap();
    set_message_spill_root(Some(spill_dir.path().join("spill")));
    // 40K CJK chars ≈ 40K estimated tokens: over the full cap.
    let original = "错".repeat(40_000);
    let inbox_backed = cap_external_message(&original, true);
    let drained = cap_external_message(&original, false);
    assert!(
        inbox_backed.contains("kallip inbox read"),
        "inbox-backed entrance must point at the inbox copy"
    );
    assert!(
        !drained.contains("kallip inbox read"),
        "drain-entrance cut must not claim an inbox copy"
    );
    assert!(inbox_backed.contains("message truncated"));
    assert!(drained.contains("message truncated"));
    set_message_spill_root(None);
}

/// Fail-open branch: with the spill root pinned to an unwritable path
/// (a regular file — creating the family dirs under it fails
/// deterministically), the oversized message is still recorded,
/// untruncated: entry protection must never cost a message.
#[tokio::test]
async fn message_guard_fails_open_when_spill_root_is_unwritable() {
    let _serial = message_spill_serial().await;
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("not-a-dir");
    std::fs::write(&root, "file").unwrap();
    set_message_spill_root(Some(root));

    let ctx = crate::test_support::make_ctx(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
    )
    .await;
    let original = "错".repeat(40_000);
    ctx.record_message_turn(&original).await;

    let store = ctx.store.lock().await;
    let content = store.turns().back().unwrap().messages[0]
        .content()
        .unwrap()
        .to_owned();
    assert_eq!(
        content, original,
        "fail-open must record the original untruncated"
    );
    set_message_spill_root(None);
}

/// A message under the cap passes through byte-for-byte: the guard only
/// exists for the oversized tail of the distribution.
#[tokio::test]
async fn message_guard_passes_ordinary_messages_byte_for_byte() {
    let ctx = crate::test_support::make_ctx(
        vec![crate::test_support::profile("test", "ep1", 4096)],
        &["ep1"],
    )
    .await;
    let text = "a normal sized prompt";
    ctx.record_message_turn(text).await;
    let store = ctx.store.lock().await;
    let content = store.turns().back().unwrap().messages[0]
        .content()
        .unwrap()
        .to_owned();
    assert_eq!(content, text);
}

// The initial-prompt exemption is structural, not behavioral: the
// initial call site inside agent_task stays on raw record_turn, and
// driving it end-to-end needs a full agent_task harness with a
// responding endpoint. The guard tests above pin the shared cut/spill
// path the exemption deliberately avoids.
