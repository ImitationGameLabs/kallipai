//! Task-ledger change watcher: subscribes to `TaskChanged` on the topic
//! bus and drops a wake hint into the prompt queue of every agent the event's
//! people set names (assignee, confirmers, creator), by identity or by the
//! role name an entry holds. The change's own agent is never woken, whether
//! an entry names it by id or by its role name.
//!
//! Verbs that already carry an explicit message face (`close`)
//! are silent here: the close summary reaches the affected agents
//! as a real message, and a synthetic hint would only duplicate it.
//! Consecutive verbs on one task collapse to one hint
//! (keep-last debounce, runs only — interleaved tasks keep their
//! order), so `start` + `note` back-to-back reads as one wakeup.
//!
//! Each target costs one inbox write, taken serially in this loop: a burst
//! larger than the topic capacity makes the receiver lag and drop hints,
//! which the next verb on the task re-announces.

use kallipai_common::agentid::AgentId;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::bus::TaskChanged;
use crate::delivery::enqueue_prompt;
use crate::state::SharedState;

/// Verbs that never wake anyone: they reach the affected agents through
/// their own explicit message face instead.
const SILENT_VERBS: [&str; 1] = ["close"];

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

/// The people set for an event: confirmers, then assignee, then creator.
/// Deduplicated, order-stable: a name listed twice keeps one entry.
fn people_of(ev: &TaskChanged) -> Vec<String> {
    let mut people: Vec<String> = ev.confirmers.clone();
    if let Some(assignee) = &ev.assignee {
        people.push(assignee.clone());
    }
    if let Some(creator) = &ev.creator {
        people.push(creator.clone());
    }
    let mut deduped: Vec<String> = Vec::new();
    for name in people {
        if !deduped.contains(&name) {
            deduped.push(name);
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

/// The agents one event wakes: every agent an entry in the people set names,
/// except the one that caused the change. An entry is an identity; one that
/// matches no agent id but equals a role name names that role's holder,
/// because a role name is the registry's readable addressing alias and the
/// registry keeps those unique. The exclusion runs on the resolved agent, so
/// an entry naming the actor by its role name does not reach it either.
fn targets_of<'a>(
    people: &[String],
    actor: &str,
    agents: impl IntoIterator<Item = (&'a AgentId, &'a str)>,
) -> Vec<AgentId> {
    let mut targets: Vec<AgentId> = Vec::new();
    for (id, role) in agents {
        if id.as_ref() == actor {
            continue;
        }
        let named = people
            .iter()
            .any(|person| person.as_str() == id.as_ref() || person.as_str() == role);
        if named {
            targets.push(id.clone());
        }
    }
    targets
}

/// Wake every registry agent the event's people set names.
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
        targets_of(
            &people,
            &ev.actor,
            registry
                .iter()
                .map(|(id, entry)| (id, entry.identity().config.role.as_str())),
        )
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

    /// Agent ids are UUIDs; the anchor keeps them readable while the id/role
    /// split stays visible in every assertion.
    fn agent(n: u128) -> AgentId {
        AgentId::from(format!("00000000-0000-4000-8000-{n:012}"))
    }

    fn event_naming(
        actor: &str,
        creator: Option<&str>,
        assignee: Option<&str>,
        confirmers: &[&str],
    ) -> TaskChanged {
        TaskChanged {
            task_id: 1,
            title: "t".into(),
            status: "in_progress".into(),
            verb: "note".into(),
            actor: actor.to_string(),
            creator: creator.map(str::to_string),
            assignee: assignee.map(str::to_string),
            confirmers: confirmers.iter().map(|name| (*name).to_string()).collect(),
        }
    }

    /// A regular verb on a task bob picked up, created by carol, reviewed by
    /// dave, and performed by eve.
    fn event(task_id: i64, verb: &str) -> TaskChanged {
        let actor = agent(5).to_string();
        let creator = agent(3).to_string();
        let assignee = agent(2).to_string();
        let reviewer = agent(4).to_string();
        TaskChanged {
            task_id,
            verb: verb.into(),
            ..event_naming(
                &actor,
                Some(&creator),
                Some(&assignee),
                &[&assignee, &reviewer],
            )
        }
    }

    /// Alice holds `dev`, bob `ops`, carol `root`: the registry keeps role
    /// names unique, so no two agents share one. A Vec, not a map — the order
    /// is what the wake assertions compare against.
    fn roster() -> Vec<(AgentId, &'static str)> {
        vec![(agent(1), "dev"), (agent(2), "ops"), (agent(3), "root")]
    }

    fn wake(ev: &TaskChanged) -> Vec<AgentId> {
        let roster = roster();
        targets_of(
            &people_of(ev),
            &ev.actor,
            roster.iter().map(|(id, role)| (id, *role)),
        )
    }

    /// Confirmers, then assignee, then creator; a name listed twice keeps one
    /// entry. One hint per agent comes from `targets_of` visiting each agent
    /// once, not from this dedupe.
    #[test]
    fn people_of_orders_confirmers_assignee_then_creator() {
        assert_eq!(
            people_of(&event(1, "start")),
            [
                agent(2).to_string(),
                agent(4).to_string(),
                agent(3).to_string()
            ]
        );
    }

    /// Whatever seat the actor holds on the task — confirmer, assignee or
    /// creator — the event it caused does not wake it.
    #[test]
    fn the_actor_never_hears_its_own_event() {
        let alice = agent(1).to_string();
        let ev = event_naming(&alice, Some(&alice), Some(&alice), &[&alice]);
        assert!(wake(&ev).is_empty());
    }

    /// The exclusion runs on the resolved agent, not on the string: an actor
    /// reached by its own role name as the assignee's free text hears nothing.
    #[test]
    fn the_actor_never_reaches_itself_by_its_role_name() {
        let alice = agent(1).to_string();
        let ev = event_naming(&alice, None, Some("dev"), &[]);
        assert!(wake(&ev).is_empty());
    }

    /// The people set carries identities: an id wakes its holder, and a
    /// comparison against role names would wake nobody.
    #[test]
    fn an_identity_in_the_people_set_wakes_its_holder() {
        let alice = agent(1).to_string();
        let actor = agent(5).to_string();
        let ev = event_naming(&actor, None, Some(&alice), &[]);
        assert_eq!(wake(&ev), [agent(1)]);
    }

    /// The creator is an identity too, and it hears about later verbs.
    #[test]
    fn an_identity_creator_wakes_its_holder() {
        let carol = agent(3).to_string();
        let actor = agent(5).to_string();
        assert_eq!(
            wake(&event_naming(&actor, Some(&carol), None, &[])),
            [agent(3)]
        );
    }

    /// An entry that matches no agent id but equals a role name names that
    /// role's holder: a free-text assignee can hold a role name.
    #[test]
    fn a_role_named_entry_wakes_its_holder() {
        let actor = agent(5).to_string();
        assert_eq!(
            wake(&event_naming(&actor, None, Some("dev"), &[])),
            [agent(1)]
        );
    }

    /// One agent reached both by id and by its role wakes once.
    #[test]
    fn an_agent_named_twice_wakes_once() {
        let alice = agent(1).to_string();
        let actor = agent(5).to_string();
        assert_eq!(
            wake(&event_naming(&actor, None, Some("dev"), &[&alice])),
            [agent(1)]
        );
    }

    /// A name that resolves to no agent produces no target.
    #[test]
    fn an_unnamed_person_wakes_nobody() {
        let actor = agent(5).to_string();
        let ev = event_naming(&actor, None, Some("nobody"), &["nobody"]);
        assert!(wake(&ev).is_empty());
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
