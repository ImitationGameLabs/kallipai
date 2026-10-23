//! The shared fact table's reading, the gateway signal's vocabulary, and what fires it.
//!
//! What the table says about the endpoint the next send would go to, the candidate the
//! chain can offer instead, and the two emitters: the gateway's auth-rejection signal
//! and the fact signal the tagma reads. The gateway signal fires at two sites: a chain
//! walk that exhausts on an auth rejection, and a round the table's verdict stops on
//! one. Both readings live here, so its vocabulary does too.

use crate::agent_task::AgentContext;
use crate::retry::decision::{Alternative, AttemptOrigin, TableReading};
use anyhow::Error;
use just_llm_client::{BackendError, provider_rejection};
use kallipai_common::facts::{BlockKind, BlockedFact, Claim, ProfilePacing, display_until};
use kallipai_common::protocol::QuotaBlockedInfo;
use kallipai_common::timefmt;
use tracing::info;

/// A gateway-level auth rejection (401/403) observed where a round ends on one:
/// at failover-chain exhaustion, or at a stop the shared table's verdict caused.
/// Lean by design: the tagma renders its own operator log and drives
/// profile-source refresh and poison state from status + code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewaySignal {
    pub status: u16,
    pub code: Option<String>,
}

/// The payload of a gateway signal: the status the upstream answered and its own
/// error code, when the response carried one.
pub(super) type SignalPayload = (u16, Option<String>);

/// The payload a settled failure carries, when it is an upstream rejection at
/// all. The error path's half of the signal: the trigger's own rejection, walked
/// out of the error chain.
pub(super) fn rejection_payload(trigger: &Error) -> Option<SignalPayload> {
    let backend = trigger.downcast_ref::<BackendError>()?;
    let rejection = provider_rejection(backend)?;
    Some((rejection.status.as_u16(), rejection.code.clone()))
}

/// Fire the gateway's signal for a rejection it acts on — 401/403, the statuses
/// that drive the tagma's profile-source refresh. One round emits at most one:
/// the error path fires when a chain walk exhausts on an auth rejection, and the
/// fact path when the shared table's verdict ends the round on one; a rejection
/// the chain recovers from stays silent. The gate lives here, so both paths hand
/// their payload to one decision. A `None` channel (local profiles, test
/// contexts) is zero behavior.
pub(super) fn emit_gateway_signal(ctx: &AgentContext, payload: SignalPayload) {
    let (status, code) = payload;
    if status != reqwest::StatusCode::UNAUTHORIZED.as_u16()
        && status != reqwest::StatusCode::FORBIDDEN.as_u16()
    {
        return;
    }
    let Some(signals) = &ctx.gateway_signals else {
        return;
    };
    let _ = signals.send(GatewaySignal { status, code });
}

/// Signal the gateway about the fact the table carried, when it is the kind that
/// says this profile's credentials were rejected. A quota or payment block parks
/// the round instead, and a shape fact is not an auth answer; the status gate
/// itself stays in the emitter. A fact stores no error code, so the signal
/// carries none.
pub(super) fn emit_fact_signal(ctx: &AgentContext, blocked: Option<&BlockedFact>) {
    let Some(block) = blocked.map(|fact| &fact.block) else {
        return;
    };
    if block.kind != BlockKind::EndpointAuth {
        return;
    }
    let Some(status) = block.status else {
        return;
    };
    emit_gateway_signal(ctx, (status, None));
}

/// What the shared table says about the profile the next send would go to: the
/// fact it carries, and whether the chain has a candidate to offer instead.
///
/// Fail-open at every step: no handle, an unknown provider, a table that cannot
/// be read — each reads as no fact, because a table that cannot be read must
/// never withhold a send.
pub(super) fn table_reading(ctx: &AgentContext, now: u64) -> TableReading {
    let Some(pacing) = ctx.profile_pacing.as_deref() else {
        return TableReading::none();
    };
    let current = ctx.failover.current_profile().id.clone();
    // The record this episode's own claim wrote is not a fact to obey: the
    // window it opened is the one the send that took it is spending. Only that
    // send, and only while the table still holds that very record: a rejection
    // another writer lands in the same second is a fact like any other.
    let own = ctx
        .episode
        .claimed_record()
        .filter(|_| ctx.episode.attempts() == 0);
    let blocked = pacing
        .paced_for(&current, now)
        .filter(|fact| own != Some(&fact.block));
    // With no fact there is nothing to escape, so no candidate is needed.
    let alternative = blocked.as_ref().and_then(|_| resolve_alternative(ctx, now));
    TableReading {
        blocked,
        alternative,
    }
}

/// Take this episode's window on the profile the next send would use, before
/// the table is read.
///
/// One window per episode, and one channel that takes it: the compaction
/// entry calls this too, so a pass that takes the window hands the same one
/// to the send that follows it. A retry inside the same episode reuses the
/// claim it already took, so a paced window is paid for once per round
/// rather than once per send. Claiming before the reading is the mechanism:
/// the record a claim writes is the one that reading sees, so a reopened
/// window lets the send go and a running one parks it down the path a fact
/// hit already takes.
///
/// An external wake forces the window open: someone asked for this round, so
/// the claim moves the window to now instead of being paced by it.
///
/// `Some(not_before)` is the window that paces this attempt — taken by
/// another sender, or by someone else's record — and `None` means the
/// episode holds the window and may send.
pub(crate) fn claim_window(
    ctx: &mut AgentContext,
    profile_id: &str,
    now: u64,
    origin: AttemptOrigin,
) -> Option<u64> {
    if ctx.episode.claimed() {
        return None;
    }
    let pacing = ctx.profile_pacing.as_deref()?;
    let force = origin == AttemptOrigin::External;
    let (claim, record) = pacing.claim(profile_id, now, force);
    match claim {
        Claim::Paced { not_before } => Some(not_before),
        Claim::Granted => {
            ctx.episode = ctx.episode.after_claim(profile_id, None);
            None
        }
        Claim::Started => {
            ctx.episode = ctx.episode.after_claim(profile_id, record);
            None
        }
    }
}

/// Give back a window this episode took and never sent against.
///
/// A claim answers for one probing send. A round that takes the window and
/// then never sends — the compaction pass that finds nothing to summarize,
/// the gate that stops before the send — leaves a record that would pace the
/// next round for a send nobody made. The window goes back, and the table
/// drops it only while it still holds the very record this claim wrote.
pub(crate) fn release_unspent_window(ctx: &mut AgentContext) {
    if ctx.episode.attempts() > 0 {
        // A send spent the window: what the table holds now answers for
        // that send, and a fact its failure wrote is not ours to drop.
        return;
    }
    let Some(window) = ctx.episode.claimed_window() else {
        return;
    };
    let profile = window.profile().to_owned();
    let record = window.record().cloned();
    if let (Some(pacing), Some(record)) = (ctx.profile_pacing.as_deref(), record) {
        pacing.release(&profile, &record);
    }
    ctx.episode = ctx.episode.after_release();
}

/// The first endpoint ahead of the active profile that could serve where the
/// current one cannot: one the shared table has nothing to say about.
///
/// The walk skips a marked candidate rather than stopping at it, so a marked
/// sibling does not hide a usable endpoint behind it. `None` means the chain has
/// nowhere to go.
///
/// The table keys one fact per profile, so a mark answers for exactly the
/// profile it names (see [`ProfilePacing::paced_for`]): every other
/// profile in the chain stays a candidate.
fn resolve_alternative(ctx: &AgentContext, now: u64) -> Option<Alternative> {
    let pacing = ctx.profile_pacing.as_deref()?;
    let mut offset = 1;
    while let Some(candidate) = ctx.failover.candidate_profile(offset) {
        if !candidate_marked(pacing, &candidate.id, now) {
            return Some(Alternative);
        }
        offset += 1;
    }
    None
}

/// Whether the shared table rules this candidate out: it carries a fact of its
/// own — and a fact names one profile, so it answers for exactly this one.
///
/// Fail-open: a table that cannot be read reads as a candidate that may serve.
pub(super) fn candidate_marked(pacing: &dyn ProfilePacing, candidate: &str, now: u64) -> bool {
    pacing.paced_for(candidate, now).is_some()
}

/// The structured reason a quota or payment block parks with: the profile it
/// names, the moment the upstream said service resumes, and the evidence.
///
/// `reset` carries the *display* face of the upstream's hint
/// ([`display_until`]), so the trust rule is applied once, here, and every
/// reader only has to tell `Some` from `None`. A context without a table
/// leaves the reset empty rather than withholding the park.
pub(super) fn quota_blocked_payload(
    ctx: &AgentContext,
    profile_id: &str,
    detail: String,
) -> QuotaBlockedInfo {
    let reset = ctx
        .profile_pacing
        .as_deref()
        .and_then(|pacing| pacing.paced_for(profile_id, timefmt::now_epoch()))
        .and_then(|fact| display_until(fact.block.recorded_at, fact.block.until));
    QuotaBlockedInfo {
        profile: profile_id.to_string(),
        reset,
        detail,
    }
}

/// The resumption sentence the fact's diagnostic readers carry: the moment the
/// upstream named for the endpoint to reopen, or that it named none. The
/// failover log line and the stop message say the same thing about the same
/// fact; the trust bound is the display faces' business, applied once where a
/// value becomes a promise a reader may show ([`display_until`]).
fn resumption_note(until: Option<u64>) -> String {
    match until {
        Some(until) => format!("upstream said it reopens at {until} (unix seconds)"),
        None => "the upstream gave no resumption time".to_owned(),
    }
}

/// The error a fact hit carries into the chain walk: no request was made, so the
/// fact is the evidence — its own words, and the moment the upstream said the
/// endpoint reopens. Shaped like `spent_budget_error`: one value carries the
/// wording to every reader (the failover log line, the stop message).
pub(super) fn fact_error(reading: &TableReading) -> anyhow::Error {
    let Some(fact) = reading.blocked.as_ref() else {
        return anyhow::anyhow!("shared fact: the table has nothing to say");
    };
    let resumption = resumption_note(fact.block.until);
    anyhow::anyhow!(
        "shared fact {:?} on this endpoint: {resumption}; evidence: {}",
        fact.block.kind,
        fact.block.detail
    )
}

/// Why a fact hit stops without walking the chain: there is no endpoint left to
/// try. Three situations produce that, and the note names the one that applies —
/// a set with nothing to walk, a chain already at its last profile, or a chain
/// whose candidates are all marked in the shared table.
pub(super) fn fact_chain_note(ctx: &AgentContext) -> &'static str {
    if ctx.failover.profile_count() == 1 {
        return "no failover attempted: the set has a single profile";
    }
    if ctx.failover.profile_idx() + 1 >= ctx.failover.profile_count() {
        return "no failover attempted: the chain is already at its last profile";
    }
    "no failover attempted: every profile ahead is marked in the shared fact table"
}

/// The operator-facing message for a stop the shared table caused: which endpoint
/// and profile stopped serving, why the chain did not take over, when the
/// upstream said service resumes, and the evidence the fact carries.
///
/// Shaped like `stop_message`, whose evidence is an error: here no request was
/// made, so the fact itself is the evidence, and the resumption sentence is the
/// one `fact_error` carries: the two diagnostic faces agree about one fact.
pub(super) fn fact_stop_message(
    stop_reason: &'static str,
    ctx: &AgentContext,
    reading: &TableReading,
    chain_note: &str,
) -> String {
    let profile = ctx.failover.current_profile();
    // The write side records the upstream's reset hint for every kind it
    // writes, so a fact reaching here has a moment to report whenever the
    // upstream named one. The sentence is the one fact_error carries: one
    // fact, one wording, on both diagnostic faces.
    let Some(fact) = reading.blocked.as_ref() else {
        return format!(
            "{}: endpoint {} (profile {}); {}; evidence: the shared table has nothing to say",
            stop_reason, profile.endpoint, profile.id, chain_note
        );
    };
    format!(
        "{}: endpoint {} (profile {}); {}; {}; evidence: {}",
        stop_reason,
        profile.endpoint,
        profile.id,
        chain_note,
        resumption_note(fact.block.until),
        fact.block.detail
    )
}

/// The observation line a skipped send leaves: the fact hit rate is counted from
/// these lines (one per skipped send), so it names the agent, the profile it
/// would have used, the moment the upstream said service reopens, and why the
/// send did not happen.
pub(super) fn log_fact_skip(ctx: &AgentContext, reading: &TableReading, reason: &str) {
    let Some(fact) = reading.blocked.as_ref() else {
        return;
    };
    info!(
        agent = ?ctx.config.agent_id,
        profile = %fact.profile,
        until = ?fact.block.until,
        skip_reason = reason,
        "shared fact skipped a send"
    );
}
