//! The per-epoch crypto state for the relay's AEAD channel to the app.
//!
//! Ported verbatim from the former standalone connector. The single
//! `Mutex<CryptoState>` (held by [`crate::relay::Inner`]) covers the session
//! key and both sequence counters so an emit always reads a key/counter pair
//! from the same epoch — a re-KEX rotates the key and resets the counters
//! atomically under this lock.

use kallip_e2ee::SessionKey;

/// The per-epoch crypto state, mutated atomically under one lock.
pub(super) struct CryptoState {
    /// Per-conversation AEAD key, established (and rotated on re-KEX) at key
    /// exchange. `None` before the first successful KEX.
    pub(super) key: Option<SessionKey>,
    /// Outgoing sequence counter. Reset to 0 on every KEX.
    pub(super) outbound_seq: u64,
    /// Highest inbound (app→tagma) `sequence_n` seen THIS crypto epoch.
    /// `None` = no message has arrived yet in the epoch (also the value a KEX
    /// resets to). `Option` (not `u64`) is load-bearing: the first message of a
    /// fresh epoch legitimately carries `sequence_n = 0`, and a plain `u64`
    /// initialized to 0 would reject it (`0 <= 0`). Cross-epoch replay of an
    /// old-key ciphertext is caught because the KEX rotated `key` (read under
    /// the same lock as this field) before the replay arrives, so AEAD decrypt
    /// fails — the window only needs to cover within-epoch replay.
    pub(super) seen_inbound: Option<u64>,
}

impl CryptoState {
    pub(super) fn new() -> Self {
        Self {
            key: None,
            outbound_seq: 0,
            seen_inbound: None,
        }
    }
}
