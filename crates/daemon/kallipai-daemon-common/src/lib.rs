//! Wire types and shared on-disk predicates for the kallipai local daemon.
//!
//!
//! Shared by the daemon, `kallipctl`, and a future web proxy. Mostly the
//! protocol shape and its round-trip guarantees; three modules also carry path
//! knowledge the daemon and its peers must agree on: [`socket`] resolves the
//! control socket, [`credentials`] the enrollment predicate, and [`roots`]
//! the three per-instance directory roots the daemon and the tagma both derive.

pub mod credentials;
pub mod roots;
pub mod socket;
pub mod wire;
