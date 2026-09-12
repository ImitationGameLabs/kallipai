//! Blob id encoding: `sha256-` plus the full 64 lowercase hex digest
//! characters.

use sha2::{Digest, Sha256};
use std::fmt;

use crate::error::Error;

/// A content address: the algorithm prefix plus the full digest.
///
/// The id is never truncated. A shorter prefix would shrink the space an
/// adversary must collide in, while the full hex keeps safety identical
/// to hash safety with no collision-handling code.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobId(String);

impl BlobId {
    /// The algorithm prefix every blob id starts with.
    pub const PREFIX: &'static str = "sha256-";

    /// The id of a SHA-256 digest: the canonical encoding path.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(format!("{}{}", Self::PREFIX, hex::encode(digest)))
    }

    /// The content address of some bytes: hash first, then encode. The
    /// one-call form callers want when they hold the bytes and need the
    /// key (a mirror write-through computing the address it will read
    /// back later).
    pub fn for_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self::from_digest(hasher.finalize().into())
    }

    /// Validates and wraps an id string: [`Self::PREFIX`] plus exactly 64
    /// lowercase hex characters. Upper case is rejected too, so the
    /// on-disk layout stays canonical.
    pub fn parse(id: &str) -> Result<Self, Error> {
        let Some(hex_part) = id.strip_prefix(Self::PREFIX) else {
            return Err(Error::InvalidId(id.to_string()));
        };
        let well_formed = hex_part.len() == 64
            && hex_part
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !well_formed {
            return Err(Error::InvalidId(id.to_string()));
        }
        Ok(Self(id.to_string()))
    }

    /// The storage bucket: the first two hex characters of the digest,
    /// spreading blobs over 256 directories so no single one accumulates
    /// every entry.
    pub fn bucket(&self) -> &str {
        &self.0[Self::PREFIX.len()..Self::PREFIX.len() + 2]
    }

    /// The canonical string form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_round_trips_through_parse() {
        let digest = [0xab_u8; 32];
        let id = BlobId::from_digest(digest);
        assert_eq!(
            id.as_str(),
            format!("{}{}", BlobId::PREFIX, hex::encode(digest))
        );
        assert_eq!(id.bucket(), "ab");
        assert_eq!(BlobId::parse(id.as_str()).unwrap(), id);
    }

    #[test]
    fn parse_rejects_bad_ids() {
        assert!(BlobId::parse("md5-deadbeef").is_err());
        assert!(BlobId::parse("sha256-TOOSHORT").is_err());
        assert!(BlobId::parse("sha256-deadbeef").is_err());
        assert!(BlobId::parse(&format!("sha256-{}", "A".repeat(64))).is_err());
        assert!(BlobId::parse(&format!("sha256-{}", "g".repeat(64))).is_err());
    }

    #[test]
    fn for_bytes_matches_known_sha256_vectors() {
        // Standard SHA-256 vectors (NIST): the empty string and "abc".
        assert_eq!(
            BlobId::for_bytes(b"").as_str(),
            "sha256-e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            BlobId::for_bytes(b"abc").as_str(),
            "sha256-ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
