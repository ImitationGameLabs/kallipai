//! The shared `id_type!` macro for opaque UUID-string identifier newtypes.
//!
//! [`crate::agentid::AgentId`] is defined through it. Defining further id
//! newtypes here (rather than hand-writing each) keeps their impl surfaces from
//! drifting apart.
//!
//! Caller contract: the invoking crate must depend on `serde` (with `derive`)
//! and `uuid` (with `v4`), since the generated impls reference them by path
//! (`macro_rules!` resolves these at the call site).

/// Define an opaque UUID-string identifier newtype.
///
/// Generates the standard identifier surface: `Debug, Clone, PartialEq, Eq,
/// PartialOrd, Ord, Hash, Serialize, Deserialize` derives, `#[serde(transparent)]`,
/// a `random()` constructor (UUID v4), and `Display`, `AsRef<str>`,
/// `From<String>`, `From<Self> for String`, `FromStr`, and `Borrow<str>` impls.
/// The inner `String` is private, so the type is opaque without format
/// validation; construct it via the generated `random()`, `From<String>`, or
/// `FromStr`.
///
/// ```ignore
/// kallip_common::id_type! {
///     /// Unique identifier for a widget.
///     WidgetId
/// }
/// ```
#[macro_export]
macro_rules! id_type {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Generate a fresh random id (UUID v4).
            pub fn random() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                $name(s)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl std::str::FromStr for $name {
            type Err = std::convert::Infallible;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok($name(s.to_owned()))
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }
    };
}
