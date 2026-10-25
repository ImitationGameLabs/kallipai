//! The keep-window rule behind manual eviction.
//!
//! `context_evict` does not wipe the conversation: it keeps the newest suffix
//! that fits two caps — a turn count and a token slice of the effective budget
//! — and condenses everything older into the pinned summary. This module owns
//! the arithmetic and the range line; the store carries the turns and the caps,
//! and the tool owns the summary text and the reply.
//!
//! The scan runs newest → oldest and stops at whichever cap binds first, so
//! the kept suffix stays contiguous: a turn that does not fit ends the window
//! instead of being skipped. Automatic compaction ([`super::compact`]) fills a
//! *prefix* forward to build a summarizer input, and its oversized-turn case
//! degrades by slicing rather than keeping nothing — opposite direction and a
//! different failure shape, so the two scans stay separate.

use super::turn::TurnId;

/// The two caps a manual eviction keeps within.
///
/// Both come from configuration, never from the call's arguments: an agent
/// choosing its own window would make the cap meaningless. The turn count is a
/// configured number; the token slice, like the pinned budget, derives from the
/// active window. The store — the one object the tool shares with the round
/// loop — carries both, so a call reads the current pair, not a spawn copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeepCaps {
    /// Upper bound on the number of turns kept.
    pub max_turns: usize,
    /// Upper bound on the estimated tokens those turns may add up to.
    pub max_tokens: usize,
}

impl KeepCaps {
    pub(crate) fn new(max_turns: usize, max_tokens: usize) -> Self {
        Self {
            max_turns,
            max_tokens,
        }
    }
}

impl Default for KeepCaps {
    /// The state before a spawn installs the agent's pair: keep nothing, with
    /// the token slice inert — the conservative end of the rule.
    fn default() -> Self {
        Self::new(0, usize::MAX)
    }
}

/// Why the token slice kept no turn at all.
///
/// A turn cap of 0 is not a degradation: it is the operator asking for every
/// conversation turn to be condensed, so the reply reports that as a note
/// about the configuration instead of an entry here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeepDegraded {
    /// The newest turn alone costs more than the token cap.
    NewestTurnOverTokens,
}

impl KeepDegraded {
    /// The machine-readable reason reported to the caller.
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::NewestTurnOverTokens => "newest turn exceeds max_tokens",
        }
    }
}

/// What one eviction call does to the conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeepWindow {
    /// How many of the oldest conversation turns to evict.
    pub evict: usize,
    /// The surviving turns, oldest first (empty when nothing is kept).
    pub kept: Vec<TurnId>,
    /// Set when the token slice kept no turn although the turn cap allowed one.
    pub degraded: Option<KeepDegraded>,
    /// The range a summary written by this call stands for. `None` when the
    /// call condenses nothing: the summary then keeps the text it already has.
    pub range: Option<SummaryRange>,
}

impl KeepWindow {
    /// Whether the window already covers the whole conversation — the no-op
    /// case: there is nothing to condense, so the summary must stay as it is.
    pub(crate) fn keeps_everything(&self) -> bool {
        self.evict == 0
    }
}

/// Plans the window over `convo`, which lists `(id, estimated_tokens)` oldest
/// first.
///
/// Everything one eviction needs comes out of this one scan: how many turns to
/// drain, which ones survive, whether the token slice degraded, and the range a
/// summary would state.
pub(crate) fn plan_keep_window(convo: &[(TurnId, usize)], caps: KeepCaps) -> KeepWindow {
    let mut kept = 0usize;
    let mut acc = 0usize;
    for (_, tokens) in convo.iter().rev() {
        if kept == caps.max_turns {
            break;
        }
        // Saturating: the running sum is only ever compared against a cap, so
        // clamping at the maximum ends the window and can never extend it.
        let next = acc.saturating_add(*tokens);
        if next > caps.max_tokens {
            break;
        }
        acc = next;
        kept += 1;
    }
    let evict = convo.len() - kept;
    let degraded = (kept == 0 && !convo.is_empty() && caps.max_turns > 0)
        .then_some(KeepDegraded::NewestTurnOverTokens);
    let range =
        (evict > 0).then(|| SummaryRange::new(convo.iter().map(|(id, _)| *id).collect(), kept));
    KeepWindow {
        evict,
        kept: convo[evict..].iter().map(|(id, _)| *id).collect(),
        degraded,
        range,
    }
}

/// The range a pinned summary stands for: which turns it now covers and which
/// ones survived in full.
///
/// Computed from the store, never authored by the model — a summary that
/// states its own range is what keeps the model from repeating or
/// contradicting turns that are still in front of it. Only a manual eviction
/// writes one: automatic compaction replaces the summary text alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SummaryRange {
    /// Conversation turn ids before the call, oldest first.
    turns: Vec<TurnId>,
    /// How many of the newest turns survive.
    kept: usize,
}

impl SummaryRange {
    /// `turns` is the conversation before the call (oldest first); `kept` is
    /// the number of newest turns the window leaves in place.
    ///
    /// A range only describes condensed turns, so a caller that keeps every
    /// turn has nothing to state: the line would name a boundary the summary
    /// does not stand for. [`plan_keep_window`] is the only caller, and it
    /// builds a range exactly when it evicts.
    fn new(turns: Vec<TurnId>, kept: usize) -> Self {
        debug_assert!(
            kept < turns.len(),
            "a summary range covers only the turns a call condensed"
        );
        Self { turns, kept }
    }

    /// The line stating the range, as a fact about the store after the call.
    ///
    /// Pinned turns draw ids from the same counter as conversation turns, so
    /// the ids a range covers need not be consecutive. The line therefore names
    /// the boundary and counts the survivors instead of writing a span, which
    /// would claim turns that never existed.
    pub(crate) fn line(&self) -> String {
        let total = self.turns.len();
        if self.kept == 0 {
            if total == 1 {
                return format!("The only turn (#{}) is condensed here.", self.first_id());
            }
            return format!(
                "All {total} turns are condensed here; the first was #{} and the last #{}.",
                self.first_id(),
                self.last_id()
            );
        }
        let kept_first = self.turns[total - self.kept].0;
        if self.kept == 1 {
            format!(
                "Everything before turn #{kept_first} is condensed here; the newest turn follows in full."
            )
        } else {
            format!(
                "Everything before turn #{kept_first} is condensed here; the {} newest turns follow in full.",
                self.kept
            )
        }
    }

    /// The oldest turn the range covers.
    fn first_id(&self) -> u64 {
        self.turns
            .first()
            .expect("a summary range covers at least one turn")
            .0
    }

    /// The newest turn the range covers.
    fn last_id(&self) -> u64 {
        self.turns
            .last()
            .expect("a summary range covers at least one turn")
            .0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convo(sizes: &[usize]) -> Vec<(TurnId, usize)> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, tokens)| (TurnId(i as u64 + 1), *tokens))
            .collect()
    }

    fn ids(sizes: &[usize]) -> Vec<TurnId> {
        convo(sizes).into_iter().map(|(id, _)| id).collect()
    }

    #[test]
    fn the_turn_cap_binds_first_when_tokens_are_slack() {
        let window = plan_keep_window(&convo(&[10, 10, 10, 10, 10]), KeepCaps::new(2, 1_000));
        assert_eq!(window.evict, 3, "everything but the two newest turns goes");
        assert_eq!(window.kept, vec![TurnId(4), TurnId(5)]);
        assert_eq!(window.degraded, None);
    }

    #[test]
    fn the_token_cap_stops_before_the_turn_that_would_cross_it() {
        let window = plan_keep_window(&convo(&[10, 10, 10]), KeepCaps::new(10, 25));
        assert_eq!(
            window.evict, 1,
            "the oldest turn is the one that breaks the cap"
        );
        assert_eq!(window.kept, vec![TurnId(2), TurnId(3)]);
    }

    #[test]
    fn a_turn_that_does_not_fit_ends_the_window_instead_of_being_skipped() {
        let window = plan_keep_window(&convo(&[10, 1_000, 10]), KeepCaps::new(10, 100));
        assert_eq!(
            window.kept,
            vec![TurnId(3)],
            "the kept suffix stays contiguous: no holes around the oversized turn"
        );
    }

    #[test]
    fn a_newest_turn_over_the_token_cap_keeps_nothing() {
        let window = plan_keep_window(&convo(&[10, 500]), KeepCaps::new(10, 100));
        assert_eq!(window.evict, 2);
        assert!(window.kept.is_empty());
        assert_eq!(window.degraded, Some(KeepDegraded::NewestTurnOverTokens));
        assert!(
            window.range.is_some(),
            "a call that condenses everything still states the range"
        );
    }

    #[test]
    fn a_zero_turn_cap_keeps_nothing_without_calling_it_a_degradation() {
        let window = plan_keep_window(&convo(&[1, 1]), KeepCaps::new(0, 1_000));
        assert_eq!(window.evict, 2);
        assert!(
            window.degraded.is_none(),
            "condensing every turn is the configuration, not a lost cap"
        );
        assert!(!window.keeps_everything());
    }

    #[test]
    fn a_window_that_covers_every_turn_is_a_no_op() {
        let window = plan_keep_window(&convo(&[10, 10, 10]), KeepCaps::new(20, 1_000));
        assert_eq!(window.evict, 0);
        assert!(window.keeps_everything());
        assert_eq!(window.kept.len(), 3);
        assert_eq!(window.degraded, None);
        assert!(
            window.range.is_none(),
            "nothing is condensed, so no range is stated"
        );
    }

    #[test]
    fn an_empty_conversation_plans_nothing() {
        let window = plan_keep_window(&[], KeepCaps::new(0, 0));
        assert_eq!(window.evict, 0);
        assert!(window.kept.is_empty());
        assert_eq!(
            window.degraded, None,
            "an empty conversation sacrifices nothing"
        );
        assert!(window.range.is_none());
    }

    #[test]
    fn an_oversized_older_turn_ends_the_window_without_overflowing() {
        let window = plan_keep_window(&convo(&[usize::MAX, 10]), KeepCaps::new(10, 100));
        assert_eq!(
            window.kept,
            vec![TurnId(2)],
            "the sum is compared against the cap, so it may not overflow"
        );
    }

    #[test]
    fn the_range_line_names_the_boundary_and_counts_the_survivors() {
        let line = SummaryRange::new(ids(&[0, 0, 0, 0]), 2).line();
        assert!(line.contains("#3"), "the first surviving turn: {line}");
        assert!(line.contains("2 newest turns follow"), "{line}");
        assert!(
            !line.contains("#4"),
            "the survivors are counted, not spanned: {line}"
        );
        assert!(
            !line.contains("#1"),
            "an evicted turn stays unnamed: {line}"
        );
    }

    #[test]
    fn the_range_line_names_a_lone_survivor() {
        let line = SummaryRange::new(ids(&[0, 0]), 1).line();
        assert!(line.contains("#2"), "{line}");
        assert!(line.contains("the newest turn follows in full"), "{line}");
        assert!(
            !line.contains("#1"),
            "the evicted turn stays unnamed: {line}"
        );
    }

    #[test]
    fn the_range_line_counts_a_full_condense_without_spanning_it() {
        let line = SummaryRange::new(ids(&[0, 0, 0]), 0).line();
        assert!(line.contains("#1"), "the first turn: {line}");
        assert!(line.contains("#3"), "the last turn: {line}");
        assert!(line.contains(" 3 "), "the whole conversation: {line}");
        assert!(
            !line.contains('–') && !line.contains("—"),
            "a span would claim every id in between: {line}"
        );
    }

    #[test]
    fn the_range_line_states_a_lone_turn() {
        assert_eq!(
            SummaryRange::new(ids(&[0]), 0).line(),
            "The only turn (#1) is condensed here."
        );
    }

    #[test]
    fn the_range_line_carries_no_span_through_pin_gaps() {
        // A pin takes its id from the same counter as a conversation turn, so
        // the turns a range covers can hop: #1 and #7 with pins in between.
        let line = SummaryRange::new(vec![TurnId(1), TurnId(7)], 0).line();
        assert!(!line.contains("1–7"), "no run is claimed: {line}");
        assert!(!line.contains("1-7"), "no run is claimed: {line}");
    }
}
