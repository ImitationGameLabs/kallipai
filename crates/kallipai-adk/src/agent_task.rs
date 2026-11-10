//! Agent task orchestration: one task, one loop, one wake at a time.
//!
//! A task is the future [`agent_task`] owns. Six entrances bring it work: the
//! first prompt, a prompt message, [`AgentContext::notify`], and the retry, wait
//! and pacing fuses; the lifecycle cancel sits beside them and ends the task. One
//! wake is one count: [`run_and_report`] books a [`WakeDisposition`] per wake, and
//! the loop reads only that verdict — `Awake` leaves the agent free to run again,
//! `Held` says something the wake must wait out holds it, `Terminated` is a
//! lifecycle cancel.
//!
//! The files answer one question each:
//!
//! - `context`: what an agent carries between rounds, and what changes it from
//!   outside one.
//! - `rounds`: how a round runs, and where each ending lands.
//! - `task_loop`: which entrances the loop waits on, and how a task ends.
//! - `wake`: how a wake's ending is named and counted, and what may follow it.
//! - `fuses`: how a park is armed, cancelled and settled, and the three timers.
//! - `message_guard`: what an external message is cut to, and where the
//!   untouched original spills.
//!
//! The names above are the module's public face. Everything else stays inside
//! the task, except `cap_external_message`, which is also published crate-wide
//! for the runner's message path.

mod context;
mod fuses;
mod message_guard;
mod rounds;
mod task_loop;
mod wake;

pub use context::AgentContext;
pub use fuses::request_park_cancel;
pub use rounds::{RoundToken, run_and_report};
pub use task_loop::{MessagePuller, agent_task};
pub use wake::WakeDisposition;

pub(crate) use message_guard::cap_external_message;
#[cfg(test)]
pub(crate) use message_guard::{message_spill_serial, set_message_spill_root};
#[cfg(test)]
pub(crate) use wake::wake_and_drain;

#[cfg(test)]
mod tests;
