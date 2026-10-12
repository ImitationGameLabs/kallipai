//! The helper the resource modules share: the caller's ownership key.

use crate::management::UserSession;

/// The caller's ownership key: the verified session's account id, the
/// same spelling the audit rows carry for the consuming account.
pub(super) fn owner_of(usr: &UserSession) -> String {
    usr.session.user_id.to_string()
}
