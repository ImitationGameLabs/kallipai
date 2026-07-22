//! Model profile registry: provider/model selection with capability tiers.
//!
//! Provides the data model, a TOML/env config loader (returning [`ProfileConfig`]), and re-exports
//! the upstream [`ChatClient`]. The tagma builds backends and assembles them into a
//! [`ProfileRegistry`]; the runtime holds pre-built backends and does selection only.
//!
//! [`ChatClient`]: just_llm_client::ChatClient

pub mod config;
pub mod model;
pub mod registry;

pub use config::{ProfileConfig, load};
pub use just_llm_client::ChatClient;
pub use model::{Endpoint, Profile, Tier};
pub use registry::{BackendSource, ProfileRegistry};
