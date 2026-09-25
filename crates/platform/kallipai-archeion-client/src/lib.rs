//! HTTP client for the kallipai-archeion relay. See [`ArcheionClient`] for the surface.

mod client;

pub use client::{ArcheionClient, ArcheionClientBuilder};
// Re-export the shared admin DTOs so callers depend on this crate alone for the
// archeion HTTP surface. `DeviceKey` is the one e2e type surfaced (for `enroll`);
// the rest of the private-key API stays in `kallipai-e2ee`.
pub use kallipai_archeion_common::admin::{
    CreateEnrollmentCodeRequest, CreateEnrollmentCodeResponse, Page, PageQuery, PasskeySummary,
    UpdateUserRequest, UserSummary,
};
pub use kallipai_archeion_common::ids::TagmaId;
pub use kallipai_common::protocol::ApiError;
pub use kallipai_e2ee::DeviceKey;
