//! Wire types and shared on-disk predicates for the kallipai local daemon.
//!
//!
//! Shared by the daemon, `kallipctl`, and a future web proxy. Mostly the
//! protocol shape and its round-trip guarantees; two modules also carry path
//! knowledge the daemon and its peers must agree on: [`socket`] resolves the
//! control socket, [`credentials`] the enrollment predicate.

pub mod credentials;
pub mod socket;
pub mod wire;
