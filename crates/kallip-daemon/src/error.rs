//! Domain error conversions for [`ApiError`].
//!
//! [`ApiError`] itself (including its `IntoResponse` impl) lives in
//! `kallip-common`. This module adds daemon-local `From` conversions
//! so route handlers can use `.map_err(ApiError::from)`.

use kallip_common::protocol::ApiError;

/// Map domain-specific [`StoreError`](crate::skill_promote::StoreError) to [`ApiError`].
impl From<crate::skill_promote::StoreError> for ApiError {
    fn from(err: crate::skill_promote::StoreError) -> Self {
        match err {
            crate::skill_promote::StoreError::NotFound(_) => Self::not_found(err.to_string()),
            crate::skill_promote::StoreError::NotPending { .. } => Self::conflict(err.to_string()),
        }
    }
}
