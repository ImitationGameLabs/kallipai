//! Task-ledger change watcher: subscribes to `TaskChanged` on the topic
//! bus and drops a wake hint into the prompt queue of every agent whose
//! role appears in the task's people set (assignee, confirmers, creator).
//!
//! Verbs that already carry an explicit message face (`close`)
//! are silent here: the close summary reaches the affected agents
//! as a real message, and a synthetic hint would only duplicate it.
//! Consecutive verbs on one task collapse to one hint
//! (keep-last debounce, runs only — interleaved tasks keep their
//! order), so `start` + `note` back-to-back reads as one wakeup.

use kallip_common::agentid::AgentId;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::bus::TaskChanged;
use crate::delivery::enqueue_prompt;
use crate::state::SharedState;

/// Verbs that never wake anyone: they reach the affected agents through
/// their own explicit message face instead.
const SILENT_VERBS: [&str; 1] = ["close"];

/// The create verb: the creator is the authenticated identity recorded
/// at request time, so a create event is always the caller registering
/// their own task and hinting them is pure noise. Events carry the
/// actor field for consumers that need the true actor. The loss is one
/// missed hint, never data.
const CREATOR_SILENT_VERBS: [&str; 1] = ["create"];

/// What the run loop does with one `recv` outcome. Lag is recovery, not
/// death: the topic counter absorbed the overflow (same loss-on-overflow
/// semantics as the relay pump), the next event re-syncs us, and a burst
/// must not kill the watcher — only a closed bus does.
enum Flow {
    Wake(TaskChanged),
    Skip,
    Stop,
}

fn classify(result: Result<TaskChanged, RecvError>) -> Flow {
    match result {
        Ok(ev) => Flow::Wake(ev),
        Err(RecvError::Lagged(n)) => {
            warn!(lost = n, "task watcher: lagged task_changed events");
            Flow::Skip
        }
        Err(RecvError::Closed) => Flow::Stop,
    }
}

/// Run until cancelled or the bus closes. Started alongside the other
/// tagma background pumps; observes the tagma-wide shutdown token.
pub(crate) async fn run(state: SharedState) {
    let cancel = state.shutdown.clone();
    let mut rx = match state.bus.subscribe::<TaskChanged>() {
        Ok(rx) => rx,
        Err(_) => {
            warn!("task watcher: bus topic missing; not starting");
            return;
        }
    };
    loop {
        let first = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            next = rx.recv() => match classify(next) {
                Flow::Wake(ev) => ev,
                Flow::Skip => continue,
                Flow::Stop => return, // bus closed: shutting down
            },
        };
        // Drain window: collect everything already queued; the keep-last
        // collapse itself is keep_last_per_task's job, below.
        let mut queued: Vec<TaskChanged> = vec![first];
        while let Ok(next) = rx.try_recv() {
            queued.push(next);
        }
        let batch = keep_last_per_task(queued);
        for ev in batch {
            announce(&state, &ev).await;
        }
    }
}

/// The people set for an event: confirmers, then assignee, then creator (the
/// creator dropped for verbs where the creator just spoke). Deduplicated,
/// order-stable — a role listed twice hears one hint, not two.
fn people_of(ev: &TaskChanged) -> Vec<String> {
    let mut people: Vec<String> = ev.confirmers.clone();
    if let Some(assignee) = &ev.assignee {
        people.push(assignee.clone());
    }
    if let Some(creator) = &ev.creator
        && !CREATOR_SILENT_VERBS.contains(&ev.verb.as_str())
    {
        people.push(creator.clone());
    }
    let mut deduped: Vec<String> = Vec::new();
    for role in people {
        if !deduped.contains(&role) {
            deduped.push(role);
        }
    }
    deduped
}

/// Keep-last debounce: collapse consecutive events for the same task
/// into the latest, preserving cross-task order.
fn keep_last_per_task(events: Vec<TaskChanged>) -> Vec<TaskChanged> {
    let mut out: Vec<TaskChanged> = Vec::new();
    for ev in events {
        if out.last().is_some_and(|b| b.task_id == ev.task_id) {
            *out.last_mut().expect("just checked") = ev;
        } else {
            out.push(ev);
        }
    }
    out
}

/// Wake every registry agent whose role is in the event's people set.
async fn announce(state: &SharedState, ev: &TaskChanged) {
    if SILENT_VERBS.contains(&ev.verb.as_str()) {
        return;
    }
    let people = people_of(ev);
    let text = format!(
        "[task] '{}' (#{}) is now {} (kallip task show {})",
        ev.title, ev.task_id, ev.status, ev.task_id
    );
    let targets: Vec<AgentId> = {
        let registry = state.registry.read().await;
        registry
            .iter()
            .filter(|(_, entry)| people.iter().any(|r| r == &entry.identity().config.role))
            .map(|(id, _)| id.clone())
            .collect()
    };
    for id in targets {
        match enqueue_prompt(
            state,
            &id,
            text.clone(),
            "task",
            crate::delivery::DeliveryNotice::Surface,
        )
        .await
        {
            Ok(_) => {
                info!(agent = %id, task = ev.task_id, verb = %ev.verb, "task wake hint queued")
            }
            Err(err) => {
                warn!(agent = %id, task = ev.task_id, error = %err, "task wake hint dropped")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(task_id: i64, verb: &str) -> TaskChanged {
        TaskChanged {
            task_id,
            title: "t".into(),
            status: "in_progress".into(),
            verb: verb.into(),
            creator: Some("root".into()),
            assignee: Some("scout".into()),
            confirmers: vec!["scout".into(), "reviewer-c".into()],
        }
    }

    /// Confirmers, assignee, creator all present on a regular verb; a role
    /// named twice (confirmer + assignee) hears one hint.
    #[test]
    fn people_of_dedupes_and_keeps_creator() {
        assert_eq!(
            people_of(&event(1, "start")),
            ["scout", "reviewer-c", "root"]
        );
    }

    /// The creator just spoke on create: dropped from the people set, the
    /// rest intact.
    #[test]
    fn people_of_drops_creator_on_create() {
        assert_eq!(people_of(&event(1, "create")), ["scout", "reviewer-c"]);
    }

    /// Silent verbs produce no hint at all — the message face covers them.
    #[test]
    fn silent_verbs_are_close_only() {
        assert_eq!(SILENT_VERBS, ["close"]);
        for verb in ["start", "note", "confirm", "pause", "resume"] {
            assert!(!SILENT_VERBS.contains(&verb), "{verb} must wake");
        }
    }

    /// A lagged receive must read as skip-and-keep-going (the bus counter
    /// absorbed the overflow), not as a closed bus — treating lag as closed
    /// would permanently silence the notification face on the first burst,
    /// since every burst larger than the topic capacity would kill the
    /// watcher instead of just dropping frames.
    #[test]
    fn lag_is_skip_and_closed_is_stop() {
        assert!(matches!(classify(Ok(event(1, "start"))), Flow::Wake(_)));
        assert!(matches!(classify(Err(RecvError::Lagged(7))), Flow::Skip));
        assert!(matches!(classify(Err(RecvError::Closed)), Flow::Stop));
    }

    /// Keep-last debounce: consecutive events for one task collapse to the
    /// latest; interleaved tasks keep both, in order.
    #[test]
    fn keep_last_collapses_same_task_runs_only() {
        let mut late = event(1, "note");
        late.status = "review".into();
        let collapsed = keep_last_per_task(vec![event(1, "start"), late, event(2, "start")]);
        assert_eq!(collapsed.len(), 2, "task 1 run collapses to one");
        assert_eq!(collapsed[0].task_id, 1);
        assert_eq!(collapsed[0].status, "review", "last wins within a run");
        assert_eq!(collapsed[1].task_id, 2);
    }
}
