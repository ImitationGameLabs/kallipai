//! WebAuthn passkey registration + login, and session minting.
//!
//! # Clone detection & signature counters — READ BEFORE CHANGING LOGIN
//!
//! WebAuthn's signature counter (`signCount`) was designed to detect cloned
//! credentials on single-device HARDWARE authenticators. It is effectively dead
//! for modern passkeys, and a regression MUST NOT auto-revoke a credential:
//!
//! - Synced passkeys (iCloud Keychain, Google, Microsoft, 1Password, ...)
//!   intentionally share one private key across devices, so they report
//!   `signCount = 0` forever and the regression check never runs. That is
//!   correct — they are legitimately multi-device, not cloned.
//! - Platform authenticators (TPM / Secure Enclave) often keep no reliable
//!   monotonic counter; a regression is frequently a firmware quirk, not a clone.
//! - Auto-revoking on a regression is an ATTACKER-TRIGGERABLE DoS: a holder of a
//!   stale clone (counter behind the stored value) can force a regression that
//!   destroys the legitimate user's credential, while the legit user — also
//!   failing the regression — cannot self-recover.
//!
//! Accordingly, the login finish handlers (`login_finish` and
//! `login_discoverable_finish`, sharing the `finalize_login_and_mint_session`
//! tail) treat `CredentialPossibleCompromise` as DENY-THIS-LOGIN + LOG ONLY:
//! they return 401 and emit a structured warn (`user_id`, `cred_id`) for
//! monitoring. They NEVER mutate the credential. A regression already fails the
//! login regardless, so not revoking costs no real protection; a leading clone
//! is undetectable anyway.
//!
//! Then how ARE clones prevented? Structurally, not at auth time:
//! - Passkey private keys are non-exportable (locked in TPM / Secure Enclave /
//!   Android Keystore) and the sync fabric is E2EE — there is no software blob
//!   an attacker can copy short of compromising the secure enclave or sync vendor.
//! - The RP stores only PUBLIC keys, so a DB breach leaks nothing usable.
//! - Clone RECOVERY is the self-service surface in `routes/passkeys.rs`: suspect
//!   a credential? revoke it (hard-delete + audit row) from another device, then
//!   add a new one. The last-live-passkey guard blocks self-lockout on revoke.
//!
//! References: W3C webauthn-3 §7.2.4; Adam Langley, "Signature counters"
//! (imperialviolet, 2023); MojoAuth, "signCount Is Dead".
//!
//! # Identity model
//!
//! The **login id is the username** (`users.username`, normalized via
//! `crate::username`). `login_begin` resolves the user by username. Email is
//! NOT a login id and NOT collected at registration: it is an optional
//! contact/recovery channel a user links later in settings, stored in the
//! `emails` table (1:N, with a primary + verification state). WebAuthn
//! `user.name` is the username; `display_name` surfaces only as the fallback
//! WebAuthn `displayName` (when the client omits one) and in `/me`. `user.id`
//! stays the opaque pre-generated `UserId` -- the only crypto binding.
//!
//! # Ceremonies
//!
//! - **register** `begin`/`finish`: open signup (no invite). `begin` normalizes
//!   the username, synthesizes a prompt-only `display_name`, pre-generates the
//!   `UserId`, and persists a bare discoverable `RegistrationState` (+ username)
//!   on the challenge row. `finish` verifies the credential (CPU, outside the
//!   txn) through `WebauthnCore`, then in ONE transaction locks the challenge
//!   row `FOR UPDATE`, checks username uniqueness `FOR UPDATE` (`409` on
//!   conflict), inserts the user + passkey (marked `discoverable`), deletes the
//!   challenge, and mints a fresh session. A parallel double-finish on one
//!   ceremony loses on the row lock.
//! - **login** `begin`/`finish`: username-first. `begin` resolves the user by
//!   `username`, loads their passkeys, and bakes them into the ceremony state
//!   via the wrapper's `start_passkey_authentication`. `finish` verifies the
//!   assertion against that baked state (CPU, outside the txn) and advances the
//!   stored passkey via `Passkey::update_credential` inside the SAME transaction
//!   that locks the challenge `FOR UPDATE`, inserts the session, and deletes the
//!   challenge — so a parallel ceremony-id replay loses on the row lock and a
//!   transient failure cannot advance the counter without issuing a session.
//!   `update_credential` returning `None` (cred_id mismatch) is a hard 500, not
//!   a silent skip: it means the row moved under us, and issuing a session
//!   while the stored `Passkey` is out of sync with what was verified would
//!   weaken the next assertion's baseline. (This is integrity of the stored
//!   credential, not clone detection — see the section above.)
//! - **login (discoverable)** `begin`/`finish`: passwordless, usernameless.
//!   `begin` resolves NO user — it returns conditional-UI options with an empty
//!   allowList. `finish` identifies the account from the assertion's signed
//!   `userHandle`, then reuses the regular login tail (`finalize_login_and_mint_
//!   session`) so the two login paths cannot drift. The assertion itself is
//!   verified via `finish_discoverable_authentication` against the resolved
//!   user's credentials (CPU, outside the txn).
//!
//! Session ids are rotated: every register/login finish mints a brand-new
//! session token (never reuses a pre-login one), defeating session fixation.

use crate::state::SharedState;
use axum::Router;
use axum::routing::post;
use std::time::Duration;

use discoverable::{login_discoverable_begin, login_discoverable_finish};
use login::{login_begin, login_finish};
use register::{register_begin, register_finish};

mod admin_login;
mod availability;
mod discoverable;
mod login;
mod register;
mod session;

pub use admin_login::admin_login_router;
pub use availability::availability_router;

pub(crate) use register::discoverable_registration_challenge;
pub(crate) use register::register_err;
pub(crate) use session::{CeremonyBeginResponse, mint_session_row, set_cookie_response};

/// Ceremony-kind discriminators stored on `webauthn_challenges.kind`. Each
/// begin path writes its own kind so the finish handler picks the right state
/// type/resolver and the per-kind in-flight caps do not collide. Shared with
/// `routes::passkeys` (the add-passkey ceremony) and `routes::device_pairing`
/// (the cross-device pair ceremony).
///
/// Open-signup registration: enrolls a DISCOVERABLE (resident-key) credential,
/// so its challenge row holds a bare core `RegistrationState` (distinct from
/// `KIND_ADD_REGULAR`, whose non-discoverable add-passkey rehydrates a
/// `PasskeyRegistration`). DISTINCT kinds also keep the per-kind in-flight caps
/// independent and let each finish handler route a row unambiguously.
pub(crate) const KIND_REGISTER: &str = "register";
/// Authenticated non-discoverable add-passkey ceremony (rehydrates a
/// `PasskeyRegistration`; resolved by the add-passkey finish handler, not
/// register_finish). Discoverable add-passkey uses `KIND_ADD_DISCOVERABLE`.
pub(crate) const KIND_ADD_REGULAR: &str = "add_regular";
const KIND_LOGIN: &str = "login";
/// Discoverable (usernameless) login: the user is resolved at finish from the
/// assertion's `userHandle`, not supplied at begin. Distinct kind so the finish
/// handler picks the right state type (`DiscoverableAuthentication`) and
/// resolver.
const KIND_LOGIN_DISCOVERABLE: &str = "login_discoverable";
pub(crate) const KIND_PAIR: &str = "pair";

/// How long an in-flight ceremony remains valid. Browsers prompt the user
/// within this window; a stale challenge is rejected at finish and GC'd at begin.
/// Shared with `routes::passkeys` (the add-passkey ceremony TTL).
pub(crate) const CHALLENGE_TTL: Duration = Duration::from_secs(300);

/// Name of the `users.username` unique index (see migration). Matched against
/// the Postgres unique-violation message to discriminate a username-collision
/// race (-> 409) from any other unique violation in the same transaction.
const USERNAME_UNIQUE_CONSTRAINT: &str = "uniq_users_username";
/// Name of the `external_identities (provider, subject)` unique index, used
/// to discriminate a concurrent first-admin-login race (two requests both
/// take the create arm; the loser hits the index) exactly the way
/// [`USERNAME_UNIQUE_CONSTRAINT`] discriminates username races.
const EXTERNAL_IDENTITY_UNIQUE_CONSTRAINT: &str = "uniq_external_identities_provider_subject";

/// Max live (unexpired) ceremonies per username (register) / user (login).
/// Bounds `webauthn_challenges` storage growth against an attacker who spams
/// begins; the per-client rate limiter is the primary gate, this is the storage
/// bound. Count-then-insert, so the cap is soft under true concurrency.
const MAX_INFLIGHT_CEREMONIES: u64 = 16;

/// The unauthenticated, crypto-expensive ceremony BEGIN endpoints. These are
/// the ceremony-spam entry surface and the only ceremony routes the per-client
/// rate limiter should cover (see `routes::router`). A
/// begin mints the unguessable `ceremony_id` that finish then consumes, so
/// finish is transitively bounded by begin's rate limit and is NOT itself
/// rate-limited (otherwise a login ceremony would cost two tokens).
pub fn begin_router() -> Router<SharedState> {
    Router::new()
        .route("/auth/register/begin", post(register_begin))
        .route("/auth/login/begin", post(login_begin))
        .route(
            "/auth/login/discoverable/begin",
            post(login_discoverable_begin),
        )
}

/// The ceremony FINISH endpoints. Not rate-limited: each requires a real,
/// unguessable, single-use `ceremony_id` issued by a (rate-limited) begin, so
/// the verification surface here is bounded by begin's limiter.
pub fn finish_router() -> Router<SharedState> {
    Router::new()
        .route("/auth/register/finish", post(register_finish))
        .route("/auth/login/finish", post(login_finish))
        .route(
            "/auth/login/discoverable/finish",
            post(login_discoverable_finish),
        )
}

#[cfg(test)]
mod tests;
