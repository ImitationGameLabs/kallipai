//! The refusal shapes: the overlap inputs and rivals, the spawn error
//! with its guidance, and the message a refusal hands the caller.

use std::path::Path;

/// Which request input an overlap rejection names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlapInput {
    /// The request's workspace path.
    Workspace,
    /// The request's data dir path.
    DataDir,
}

impl std::fmt::Display for OverlapInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverlapInput::Workspace => f.write_str("workspace"),
            OverlapInput::DataDir => f.write_str("data dir"),
        }
    }
}

/// What an overlapping request input ran into. Paths render as the
/// request sent them (canonicalization is for the comparison only),
/// so the operator sees their own spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlapRival {
    /// The shared instance data tree.
    InstanceTree { root: String },
    /// The same request's workspace (a data dir placed under or
    /// around it).
    RequestedWorkspace { path: String },
    /// A registered instance's workspace.
    InstanceWorkspace { slug: String, path: String },
    /// A registered instance's data dir.
    InstanceDataDir { slug: String, path: String },
}

impl std::fmt::Display for OverlapRival {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverlapRival::InstanceTree { root } => {
                write!(f, "the instance tree rooted at {root}")
            }
            OverlapRival::RequestedWorkspace { path } => {
                write!(f, "the request's workspace at {path}")
            }
            OverlapRival::InstanceWorkspace { slug, path } => {
                write!(f, "instance {slug}'s workspace at {path}")
            }
            OverlapRival::InstanceDataDir { slug, path } => {
                write!(f, "instance {slug}'s data dir at {path}")
            }
        }
    }
}

impl SpawnError {
    /// A request path (workspace or data dir) vs the shared tree.
    pub(crate) fn overlaps_instance_tree(
        input: OverlapInput,
        requested: &Path,
        tree_root: &Path,
    ) -> Self {
        SpawnError::Overlap {
            input,
            requested: requested.display().to_string(),
            rival: OverlapRival::InstanceTree {
                root: tree_root.display().to_string(),
            },
        }
    }

    /// The request's data dir vs its own workspace.
    pub(crate) fn overlaps_request_workspace(requested: &Path, workspace: &Path) -> Self {
        SpawnError::Overlap {
            input: OverlapInput::DataDir,
            requested: requested.display().to_string(),
            rival: OverlapRival::RequestedWorkspace {
                path: workspace.display().to_string(),
            },
        }
    }

    /// A request path vs a registered instance's workspace.
    pub(crate) fn overlaps_instance_workspace(
        input: OverlapInput,
        requested: &Path,
        slug: &str,
        existing: &Path,
    ) -> Self {
        SpawnError::Overlap {
            input,
            requested: requested.display().to_string(),
            rival: OverlapRival::InstanceWorkspace {
                slug: slug.to_string(),
                path: existing.display().to_string(),
            },
        }
    }

    /// A request path vs a registered instance's data dir.
    pub(crate) fn overlaps_instance_data_dir(
        input: OverlapInput,
        requested: &Path,
        slug: &str,
        existing: &Path,
    ) -> Self {
        SpawnError::Overlap {
            input,
            requested: requested.display().to_string(),
            rival: OverlapRival::InstanceDataDir {
                slug: slug.to_string(),
                path: existing.display().to_string(),
            },
        }
    }
}

/// How the request can fail, mapped 1:1 onto wire error codes by the server.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("slug {0:?} already exists")]
    SlugTaken(String),
    #[error("{input} {requested} overlaps {rival}")]
    Overlap {
        /// Which request input overlaps.
        input: OverlapInput,
        requested: String,
        /// What the input ran into: the shared instance tree, the
        /// request's own workspace, or a registered instance's
        /// workspace or data dir.
        rival: OverlapRival,
    },
    #[error("{0}")]
    Invalid(String),
    /// The requester may not launch an instance as the target user.
    /// Fired before any state change — a denied requester learns
    /// nothing about slug existence from the error.
    #[error("uid {peer_uid} may not launch an instance as uid {target_uid}")]
    Denied { peer_uid: u32, target_uid: u32 },
    #[error(
        "instance did not publish pid/port within {timeout_secs}s; see the \
         instance log files under <state-home>/kallipai/tagmata/<slug>/logs/ and system OOM records"
    )]
    Timeout { timeout_secs: u64 },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// The denial guidance tail for the three per-instance verbs: the
/// record's owner may act on it, and root may act on anyone's — so
/// a root-owned record needs only one name.
pub(crate) fn deny_guidance(uid: &u32) -> &'static str {
    if *uid == 0 {
        "run as root"
    } else {
        "run as the owner or root"
    }
}
