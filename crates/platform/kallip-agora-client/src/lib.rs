//! HTTP client for the kallip-agora relay. See [`AgoraClient`] for the surface.

mod client;

pub use client::{AgoraClient, AgoraClientBuilder};
// Re-export the shared admin DTOs so callers depend on this crate alone for the
// agora HTTP surface. `DeviceKey` is the one e2e type surfaced (for `enroll`);
// the rest of the private-key API stays in `kallip-e2ee`.
pub use kallip_agora_common::admin::{
    CreateEnrollmentCodeRequest, CreateEnrollmentCodeResponse, Page, PageQuery, PasskeySummary,
    UpdateUserRequest, UserSummary,
};
pub use kallip_agora_common::ids::TagmaId;
pub use kallip_common::protocol::ApiError;
pub use kallip_e2ee::DeviceKey;
