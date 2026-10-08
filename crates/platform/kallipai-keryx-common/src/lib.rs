//! Wire types for the `kallipai-keryx` relay service: the tunnel inbound
//! frames ([`tunnel`]) and the data-plane key-exchange wire ([`control`]).
//!
//! The keryx service, the tagma relay, and the `kallipai-lesche-client`
//! crate depend on these wire types; the lesche service itself does not.
//! The shared foundation the frames build on (the E2E
//! [`kallipai_lesche_common::message`] envelope and the signed-proof
//! transcripts in [`kallipai_lesche_common::proof`]) lives in
//! `kallipai-lesche-common`, which this crate depends on one-way so the
//! keryx wire never forks the shared envelope family.
//!
//! Like the lesche crate, the crypto material is opaque here: no private-key
//! material ever lives in this crate.

pub mod control;
pub mod event;
pub mod tunnel;
