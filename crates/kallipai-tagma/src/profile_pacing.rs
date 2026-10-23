//! The shared upstream-fact table (see `kallipai_common::facts`): one
//! tagma-wide view of which profiles the upstream is refusing, keyed by the
//! profile id an agent runs.
//!
//! One record per profile: a failure is about the profile that met it, and a
//! sibling is a candidate again the moment the chain reaches it. Nothing here
//! looks at the config — a record for a profile it no longer knows expires by
//! time like any other, which keeps this table from failing a round.
//!
//! Fail-open by construction: every read is a snapshot of what is readable
//! *now* — an expired fact simply is not there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kallipai_common::facts::{
    Block, BlockedFact, Claim, FACT_TABLE_CAPACITY, FactView, ProfilePacing, claim_window,
    display_until, is_active,
};

/// The tagma-wide fact table. One instance per tagma, shared by every agent
/// (cloned into each `AgentContext`) and read by the status surfaces.
pub struct ProfilePacingTable {
    facts: Mutex<HashMap<String, Block>>,
}

impl ProfilePacingTable {
    /// The one construction point (the tagma's `AppState`).
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            facts: Mutex::new(HashMap::new()),
        })
    }

    /// The table lock, poison-tolerant: a panicking writer must not be able to
    /// disable the table for the rest of the process (same recovery as
    /// `TokenBudget`).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Block>> {
        self.facts.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ProfilePacing for ProfilePacingTable {
    fn record(&self, profile_id: &str, block: Block) {
        insert_fact(&mut self.lock(), profile_id.to_string(), block);
    }

    fn claim(&self, profile_id: &str, now: u64, force: bool) -> Claim {
        let mut facts = self.lock();
        let (claim, renewed) = claim_window(facts.get(profile_id), now, force);
        if let Some(block) = renewed {
            facts.insert(profile_id.to_string(), block);
        }
        claim
    }

    fn note_success(&self, profile_id: &str) {
        self.lock().remove(profile_id);
    }

    fn release(&self, profile_id: &str, started_at: u64) {
        let mut facts = self.lock();
        if facts
            .get(profile_id)
            .is_some_and(|block| block.recorded_at == started_at)
        {
            facts.remove(profile_id);
        }
    }

    fn paced_for(&self, profile_id: &str, now: u64) -> Option<BlockedFact> {
        let facts = self.lock();
        let block = facts.get(profile_id)?;
        // The one hit predicate, applied on the read side: an expired fact is
        // not there.
        if !is_active(block, now) {
            return None;
        }
        Some(BlockedFact {
            block: block.clone(),
            profile: profile_id.to_string(),
        })
    }

    fn snapshot(&self, now: u64) -> Vec<FactView> {
        let mut views: Vec<FactView> = self
            .lock()
            .iter()
            .filter_map(|(profile, block)| {
                // The one hit predicate, applied on the read side: what is not
                // readable now never reaches a surface.
                if !is_active(block, now) {
                    return None;
                }
                Some(FactView {
                    kind: block.kind,
                    until: display_until(block.recorded_at, block.until),
                    detail: block.detail.clone(),
                    recorded_at: block.recorded_at,
                    profile: profile.clone(),
                })
            })
            .collect();
        // Newest first, then the profile id, then the kind: two records made in
        // the same second still render in one order across runs.
        views.sort_by(|a, b| {
            b.recorded_at
                .cmp(&a.recorded_at)
                .then_with(|| a.profile.cmp(&b.profile))
                .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
        });
        views
    }
}

/// Insert one fact, evicting the oldest by `recorded_at` when a new key arrives
/// at capacity. Free-standing so the capacity rule is testable without a table
/// around it.
fn insert_fact(facts: &mut HashMap<String, Block>, profile_id: String, block: Block) {
    if !facts.contains_key(&profile_id)
        && facts.len() >= FACT_TABLE_CAPACITY
        && let Some(oldest) = facts
            .iter()
            .min_by_key(|(_, block)| block.recorded_at)
            .map(|(profile, _)| profile.clone())
    {
        facts.remove(&oldest);
    }
    facts.insert(profile_id, block);
}

#[cfg(test)]
mod tests {
    use super::*;
    use kallipai_common::facts::{
        BlockKind, MIN_RETRY_INTERVAL_SECS, PAYMENT_RETRY_INTERVAL_SECS, QUOTA_RESET_TRUST_MAX_SECS,
    };

    const T0: u64 = 1_700_000_000;

    fn table() -> Arc<ProfilePacingTable> {
        ProfilePacingTable::new()
    }

    fn block(kind: BlockKind, recorded_at: u64, until: Option<u64>) -> Block {
        Block {
            kind,
            until,
            detail: "HTTP 429".into(),
            status: None,
            recorded_at,
        }
    }

    #[test]
    fn a_record_answers_for_its_own_profile_only() {
        let table = table();
        table.record("a", block(BlockKind::Quota, T0, None));

        let fact = table
            .paced_for("a", T0)
            .expect("the profile carries the record");
        assert_eq!(fact.block.kind, BlockKind::Quota);
        assert_eq!(fact.profile, "a");
        assert!(
            table.paced_for("b", T0).is_none(),
            "a sibling profile is not covered by another profile's record"
        );
    }

    #[test]
    fn a_newer_record_on_one_profile_replaces_the_older_one() {
        let table = table();
        table.record("a", block(BlockKind::EndpointShape, T0, None));
        table.record("a", block(BlockKind::Quota, T0 + 10, None));

        let views = table.snapshot(T0 + 10);
        assert_eq!(views.len(), 1, "one profile, one record");
        assert_eq!(views[0].kind, BlockKind::Quota);
        assert_eq!(views[0].recorded_at, T0 + 10);
    }

    #[test]
    fn an_expired_record_is_not_snapshotted() {
        let table = table();
        table.record("a", block(BlockKind::Quota, T0, None));

        assert_eq!(table.snapshot(T0 + MIN_RETRY_INTERVAL_SECS - 1).len(), 1);
        assert!(table.snapshot(T0 + MIN_RETRY_INTERVAL_SECS).is_empty());
    }

    #[test]
    fn an_expired_record_is_not_read() {
        let table = table();
        table.record("a", block(BlockKind::Quota, T0, Some(T0 + 60)));

        assert!(table.paced_for("a", T0 + 59).is_some());
        assert!(
            table.paced_for("a", T0 + 60).is_none(),
            "the upstream's own moment ends the window"
        );

        table.record("a", block(BlockKind::Quota, T0, None));
        assert!(
            table.paced_for("a", T0 + MIN_RETRY_INTERVAL_SECS).is_none(),
            "without a hint the kind's interval is the whole window"
        );
    }

    #[test]
    fn a_second_claim_inside_the_window_leaves_the_record_where_it_was() {
        let table = table();
        table.record("a", block(BlockKind::Quota, T0, None));

        assert_eq!(
            table.claim("a", T0 + 1, false),
            Claim::Paced {
                not_before: T0 + MIN_RETRY_INTERVAL_SECS
            }
        );
        assert_eq!(
            table.snapshot(T0 + 1)[0].recorded_at,
            T0,
            "a paced claim does not move the window"
        );
        assert!(
            table
                .paced_for("a", T0 + MIN_RETRY_INTERVAL_SECS - 1)
                .is_some()
        );
        assert!(table.paced_for("a", T0 + MIN_RETRY_INTERVAL_SECS).is_none());
    }

    #[test]
    fn a_success_drops_the_record_and_the_next_claim_is_free() {
        let table = table();
        table.record("a", block(BlockKind::Quota, T0, Some(T0 + 600)));
        assert!(matches!(
            table.claim("a", T0 + 1, false),
            Claim::Paced { .. }
        ));

        table.note_success("a");
        assert!(table.paced_for("a", T0 + 1).is_none());
        assert_eq!(table.claim("a", T0 + 1, false), Claim::Granted);
        assert!(
            table.snapshot(T0 + 1).is_empty(),
            "an unmarked profile is not written down by a claim"
        );
    }

    #[test]
    fn a_claim_past_the_window_starts_a_new_one() {
        let table = table();
        table.record("a", block(BlockKind::Quota, T0, None));
        let later = T0 + MIN_RETRY_INTERVAL_SECS;

        assert_eq!(table.claim("a", later, false), Claim::Started);
        let fact = table.paced_for("a", later).unwrap();
        assert_eq!(fact.block.recorded_at, later);
        assert_eq!(fact.block.until, None);
        assert!(
            table
                .paced_for("a", later + MIN_RETRY_INTERVAL_SECS)
                .is_none()
        );
    }

    #[test]
    fn a_payment_record_waits_longer_than_a_quota_one() {
        let table = table();
        table.record("a", block(BlockKind::Payment, T0, None));
        table.record("b", block(BlockKind::Quota, T0, None));

        assert!(
            table
                .paced_for("a", T0 + PAYMENT_RETRY_INTERVAL_SECS - 1)
                .is_some()
        );
        assert!(
            table
                .paced_for("b", T0 + PAYMENT_RETRY_INTERVAL_SECS - 1)
                .is_none()
        );
    }

    #[test]
    fn a_reset_beyond_the_trust_bound_displays_as_unknown() {
        let table = table();
        let far = T0 + QUOTA_RESET_TRUST_MAX_SECS + 60;
        table.record("a", block(BlockKind::Quota, T0, Some(far)));
        assert_eq!(table.snapshot(T0)[0].until, None);

        let near = T0 + 600;
        table.record("a", block(BlockKind::Quota, T0, Some(near)));
        assert_eq!(table.snapshot(T0)[0].until, Some(near));
    }

    #[test]
    fn capacity_evicts_the_oldest_fact() {
        let mut facts = HashMap::new();
        for i in 0..FACT_TABLE_CAPACITY as u64 {
            insert_fact(
                &mut facts,
                format!("p{i:04}"),
                block(BlockKind::Quota, T0 + i, None),
            );
        }
        let newest = "newest".to_string();
        insert_fact(
            &mut facts,
            newest.clone(),
            block(BlockKind::Quota, T0 + 10_000, None),
        );

        assert_eq!(facts.len(), FACT_TABLE_CAPACITY);
        assert!(facts.contains_key(&newest));
        assert!(!facts.contains_key("p0000"), "the oldest record is evicted");
    }

    #[test]
    fn a_record_outlives_the_profile_it_names() {
        // Nothing here consults the config: a record for a profile this tagma
        // no longer runs stays readable until its window ends, then leaves.
        let table = table();
        table.record("gone", block(BlockKind::EndpointAuth, T0, None));

        assert!(table.paced_for("gone", T0).is_some());
        assert!(
            table
                .paced_for("gone", T0 + MIN_RETRY_INTERVAL_SECS)
                .is_none()
        );
    }

    #[test]
    fn a_view_serializes_exactly_the_fields_it_has() {
        let table = table();
        table.record("a", block(BlockKind::Payment, T0, None));

        let json = serde_json::to_string(&table.snapshot(T0)).expect("views serialize");
        assert_eq!(
            json,
            r#"[{"kind":"payment","detail":"HTTP 429","recorded_at":1700000000,"profile":"a"}]"#
        );
    }

    /// Records made in the same second share a `recorded_at`, so the order has
    /// to come from the profile and then the kind. Three records, one second —
    /// the tie-breaks decide, and the assertion pins the whole order.
    #[test]
    fn records_made_in_the_same_second_render_in_one_order() {
        let table = table();
        table.record("c", block(BlockKind::EndpointShape, T0, None));
        table.record("a", block(BlockKind::EndpointShape, T0, None));
        table.record("b", block(BlockKind::EndpointAuth, T0, None));

        let order: Vec<(String, BlockKind)> = table
            .snapshot(T0)
            .into_iter()
            .map(|view| (view.profile, view.kind))
            .collect();
        assert_eq!(
            order,
            vec![
                ("a".to_owned(), BlockKind::EndpointShape),
                ("b".to_owned(), BlockKind::EndpointAuth),
                ("c".to_owned(), BlockKind::EndpointShape),
            ],
            "the profile decides first, the kind breaks nothing here"
        );
    }

    #[test]
    fn a_record_made_after_the_first_one_renders_first() {
        let table = table();
        table.record("a", block(BlockKind::Quota, T0, None));
        table.record("b", block(BlockKind::Quota, T0 + 5, None));

        let profiles: Vec<String> = table
            .snapshot(T0 + 5)
            .into_iter()
            .map(|view| view.profile)
            .collect();
        assert_eq!(profiles, ["b", "a"], "newest first");
    }
}
