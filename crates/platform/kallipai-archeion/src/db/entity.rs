//! sea-orm entity models for the durable tables. The migrations under
//! [`super::migration`] prime the full schema.

pub mod device_pairing_codes;
pub mod emails;
pub mod external_identities;
pub mod oauth_states;
pub mod passkey_revocations;
pub mod passkeys;
pub mod sessions;
pub mod tagma_tokens;
pub mod tagmata;
pub mod users;
pub mod webauthn_challenges;
