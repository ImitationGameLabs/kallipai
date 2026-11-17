//! The lesche surface: the root agent's user-facing communication, outward to
//! the end user, to a joined room, or to a peer tagma's direct session, and the
//! reads that answer back (a room's history, a direct session's history, the
//! rooms this tagma can address, the unified session list). Sending as the
//! agent is root-only: that voice belongs to the root agent, so a subagent that
//! tries is rejected and routes outward communication through its supervisor.
//! The two reads the console reaches through the manage bridge, the session list
//! and the direct-session history, also answer the operator. The agent drives it
//! with `kallip lesche send` over `bash_exec`, authenticating with its own
//! per-agent token; the tagma, holding the E2E key in-process, delivers the
//! text as an `AssistantContent` envelope over the relay.
//!
//! The handlers live in submodules grouped by what they act on: `send` (the one
//! send primitive and its two non-bilateral targets), `room` (the rooms this
//! tagma can address and one room's history), `direct` (the two direct-session
//! reads), `sessions` (the unified session list), `render` (the text blocks and
//! JSON rows both reads answer with) and `shared` (the guards, the
//! negative-position refusal, and the error mapping). The re-exports below keep
//! the names the two route tables mount resolving under
//! `crate::routes::lesche::`.
//!
//! The types named in the handler signatures keep crate visibility: the two
//! route tables expand those signatures in another module, and a narrower
//! visibility would refuse the mount.

mod direct;
mod render;
mod room;
mod send;
mod sessions;
mod shared;
#[cfg(test)]
mod tests;
mod thread;

pub use direct::{read_direct_session_messages, read_direct_session_messages_by_id};
pub use room::{list_joined_rooms, read_room_messages};
pub use send::post_message;
pub use sessions::list_lesche_sessions;
pub use thread::{delete_thread_subscription, get_thread_subscription, put_thread_subscription};
