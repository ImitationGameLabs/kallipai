//! Collection references: the account-scoped container a name lives in.
//!
//! A profile-set name is unique inside its collection, never across the
//! gateway face. The mirror records which collection it serves, so `GET
//! /profiles` answers the `owner/name` pair and the CLI prints it —
//! `Display` is the CLI's spelling; the answer serializes the two fields.

use std::fmt;

use serde::{Deserialize, Serialize};

/// One collection address: an owner's collection name.
///
/// Both halves are opaque to the tagma — it never interprets them. The
/// mirror records the collection it is served from, and the reference is
/// what `GET /profiles` answers and what the CLI prints beside the count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionRef {
    /// The account the collection belongs to (the gateway's `owner`).
    pub owner: String,
    /// The collection's name inside that account.
    pub collection_name: String,
}

impl CollectionRef {
    pub fn new(owner: impl Into<String>, collection_name: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            collection_name: collection_name.into(),
        }
    }
}

impl fmt::Display for CollectionRef {
    /// `owner/name` — the spelling the CLI prints.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.collection_name)
    }
}
