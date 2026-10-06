//! The forwarding face's shared pieces: the profile-override header and
//! the request body bound. The forwarding itself lives in `data_plane`
//! (the `ProxyHttp` phases); what stays here is the family-independent
//! support -- the resolution gates' vocabulary is spelled there.
//!
//! The per-family knowledge (wire paths, credential shapes, hand-built
//! error envelopes) is `dialect`.

pub(crate) mod dialect;

/// The optional explicit profile override header (decision 2: needed only
/// to address one profile inside a served set's failover order).
pub const PROFILE_HEADER: &str = "x-kallipai-profile";

/// Request body bound: 8 MB -- generous for chat completions, bounded so a
/// runaway client cannot balloon proxy memory.
pub(crate) const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
