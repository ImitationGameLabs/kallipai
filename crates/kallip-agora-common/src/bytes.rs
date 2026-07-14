//! Opaque byte newtypes serialized as base64 on the wire.
//!
//! These carry AEAD ciphertext and public-key/signature material. The agora
//! forwards them without interpreting the bytes; length/structure validation is
//! the job of the crypto layer in the herald/app, not the wire crate.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    STANDARD.encode(bytes).serialize(serializer)
}

fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let s = String::deserialize(deserializer)?;
    STANDARD.decode(s).map_err(serde::de::Error::custom)
}

macro_rules! base64_bytes {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name(pub Vec<u8>);

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serialize(&self.0, serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserialize(deserializer).map(Self)
            }
        }
    };
}

base64_bytes! {
    /// AEAD ciphertext (incl. the Poly1305 tag) carried inside an
    /// [`Envelope`](crate::message::Envelope). Opaque to the agora; decrypted
    /// only by the receiving endpoint.
    Ciphertext
}
base64_bytes! {
    /// An Ed25519 public key (32 bytes) pinned to a team at enrollment and used
    /// to verify key-exchange signatures.
    Ed25519PublicKey
}
base64_bytes! {
    /// An Ed25519 signature (64 bytes) over a key-exchange transcript.
    Ed25519Signature
}
base64_bytes! {
    /// An X25519 ephemeral public key (32 bytes) contributed by one endpoint in
    /// a key exchange.
    X25519PublicKey
}
base64_bytes! {
    /// An opaque byte buffer for the HTTP tunnel: a tunneled request body or a
    /// streamed response chunk. Base64 (not a JSON number array) so frames stay
    /// compact under the agora's request-body limit. Opaque to the relay, which
    /// never decrypts the enclosing [`TunnelFrame`](crate::message::TunnelFrame).
    B64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ciphertext_round_trips_as_base64() {
        let original = Ciphertext(vec![0xde, 0xad, 0xbe, 0xef]);
        let json = serde_json::to_string(&original).unwrap();
        // base64("deadbeef") == "3q2+7w=="
        assert_eq!(json, "\"3q2+7w==\"");
        let back: Ciphertext = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn invalid_base64_is_rejected() {
        assert!(serde_json::from_str::<Ciphertext>("\"not!!base64\"").is_err());
    }
}
