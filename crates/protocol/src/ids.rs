//! Typed identifiers.
//! # Why newtypes rather than `String`
//! Every id in this system is a UUID string, which means `String` lets you pass a turn id
//! where a session id belongs and the compiler stays quiet. These types cost nothing at
//! runtime (a `Uuid` is 16 bytes) and make that mistake unrepresentable.
//! The wire form is unchanged: every id serializes as its plain string, so the protocol
//! boundary still only ever carries strings — the "protocol only sees string ids" rule is
//! about the *wire*, not about the Rust type.
//! # Stored as TEXT, not BLOB
//! A UUID is 36 bytes as TEXT and 16 as BLOB. We choose TEXT because `sqlite3 state.db
//! 'select * from session'` has to be readable by a human debugging a live problem, and
//! because ids appear in log lines that must be greppable against the database. At our row
//! counts the 20 bytes are irrelevant; the debuggability is not.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Error from parsing an id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed {kind}: {value:?}")]
pub struct IdParseError {
    pub kind: &'static str,
    pub value: String,
}

/// Declares a UUID-backed identifier.
/// Generates `Display`, `FromStr`, serde (as a plain string), `new()` for a fresh v7 and —
/// under the `sql` feature — `ToSql`/`FromSql`. The SQL impls live here rather than in the
/// store crate because the orphan rule forbids implementing a foreign trait for a foreign
/// type: the id types are ours, so the impls have to be too.
#[macro_export]
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(feature = "ts", derive(::ts_rs::TS), ts(type = "string"))]
        pub struct $name(pub ::uuid::Uuid);

        // Hand-written rather than `#[serde(with = ...)]`: a `with` path must be a literal,
        // and a macro-expanded `$crate::…` string is not accepted there.
        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(
                &self,
                s: S,
            ) -> ::std::result::Result<S::Ok, S::Error> {
                s.collect_str(&self.0)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                d: D,
            ) -> ::std::result::Result<Self, D::Error> {
                let s = <::std::string::String as ::serde::Deserialize>::deserialize(d)?;
                s.parse().map_err(::serde::de::Error::custom)
            }
        }

        impl $name {
            /// A fresh id. **v7**: time-ordered, so primary-key inserts append instead of
            /// scattering, and time-range queries can use the index.
            pub fn new() -> Self {
                Self(::uuid::Uuid::now_v7())
            }

            pub const fn from_uuid(u: ::uuid::Uuid) -> Self {
                Self(u)
            }

            pub const fn as_uuid(&self) -> &::uuid::Uuid {
                &self.0
            }

            pub const KIND: &'static str = stringify!($name);
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                ::std::fmt::Display::fmt(&self.0, f)
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::ids::IdParseError;
            fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
                ::uuid::Uuid::parse_str(s)
                    .map(Self)
                    .map_err(|_| $crate::ids::IdParseError {
                        kind: stringify!($name),
                        value: s.to_string(),
                    })
            }
        }

        impl From<::uuid::Uuid> for $name {
            fn from(u: ::uuid::Uuid) -> Self {
                Self(u)
            }
        }

        #[cfg(feature = "sql")]
        impl ::rusqlite::ToSql for $name {
            fn to_sql(&self) -> ::rusqlite::Result<::rusqlite::types::ToSqlOutput<'_>> {
                Ok(::rusqlite::types::ToSqlOutput::from(self.0.to_string()))
            }
        }

        #[cfg(feature = "sql")]
        impl ::rusqlite::types::FromSql for $name {
            fn column_result(
                v: ::rusqlite::types::ValueRef<'_>,
            ) -> ::rusqlite::types::FromSqlResult<Self> {
                let s = v.as_str()?;
                s.parse().map_err(|_| {
                    ::rusqlite::types::FromSqlError::Other(Box::new($crate::ids::IdParseError {
                        kind: stringify!($name),
                        value: s.to_string(),
                    }))
                })
            }
        }
    };
}

/// Declares the wire form of a fieldless enum: `as_str`, `parse_str`, `Display`, `ALL`.
/// The wire name is written out per variant rather than derived, because renaming a Rust
/// variant must not silently invalidate every row already on disk.
/// This emits **no** SQL impls — see [`impl_enum_sql`], which is separate because a
/// `#[cfg(feature = "sql")]` inside a macro resolves against the *invoking* crate's features,
/// not this one's. Bundling them made the impls silently vanish in `zlogic-store`.
#[macro_export]
macro_rules! define_enum_wire {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)+
                }
            }

            pub fn parse_str(s: &str) -> ::std::option::Option<Self> {
                match s {
                    $($wire => Some(Self::$variant),)+
                    _ => None,
                }
            }

            /// Every variant, for exhaustiveness tests.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

    };
}

/// Adds `ToSql`/`FromSql` for an enum that already has [`define_enum_wire`].
/// Invoke it from a crate that depends on rusqlite. Kept separate from `define_enum_wire`
/// so that the impls cannot be accidentally cfg'd out: the feature gate has to live at the
/// invocation site, where it actually refers to the right crate's features.
#[macro_export]
macro_rules! impl_enum_sql {
    ($name:ty) => {
        impl ::rusqlite::ToSql for $name {
            fn to_sql(&self) -> ::rusqlite::Result<::rusqlite::types::ToSqlOutput<'_>> {
                Ok(::rusqlite::types::ToSqlOutput::from(self.as_str()))
            }
        }

        impl ::rusqlite::types::FromSql for $name {
            fn column_result(
                v: ::rusqlite::types::ValueRef<'_>,
            ) -> ::rusqlite::types::FromSqlResult<Self> {
                let s = v.as_str()?;
                // An unknown value must be loud. Silently mapping it to a default would make
                // a row written by a newer schema look like ordinary data.
                <$name>::parse_str(s).ok_or_else(|| {
                    ::rusqlite::types::FromSqlError::Other(Box::new($crate::ids::IdParseError {
                        kind: stringify!($name),
                        value: s.to_string(),
                    }))
                })
            }
        }
    };
}

define_id! {
    /// A conversation. Sub-agent runs get their own.
    SessionId
}
define_id! {
    /// One user submission and everything the agent did in response.
    TurnId
}
define_id! {
    /// One LLM request plus the client-side tool batch it produced.
    RoundId
}
define_id! {
    /// One row in `session_entry`.
    EntryId
}
define_id! {
    /// One user submission sitting in the mailbox.
    SubmissionId
}
define_id! {
    /// A scope for configuration and resources; usually bound to a directory.
    WorkspaceId
}
define_id! {
    /// One recorded usage event.
    UsageId
}
define_id! {
    /// One durable user memory.
    MemoryId
}
define_id! {
    /// One user-managed external database, object store, or cloud account.
    ResourceId
}
define_id! {
    /// One append-only change to a durable user memory.
    MemoryEventId
}
define_id! {
    /// Identifies a single acquisition of a session lock. Guards against pid reuse:
    /// a revived process must not be able to pass itself off as the still-live holder.
    HolderId
}

/// A tool call id. **Not a UUID** — it comes from the provider (`call_abc`, `toolu_01A`,
/// `fc_1`) and has to be zlogiced back verbatim, so it stays an opaque string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", derive(::ts_rs::TS), ts(type = "string"))]
pub struct CallId(pub String);

impl CallId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for CallId {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

impl From<String> for CallId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

#[cfg(feature = "sql")]
impl rusqlite::ToSql for CallId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.0.as_str()))
    }
}

#[cfg(feature = "sql")]
impl rusqlite::types::FromSql for CallId {
    fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        Ok(Self(v.as_str()?.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_time_ordered() {
        // The whole point of v7: primary keys append instead of scattering.
        let ids: Vec<SessionId> = (0..64).map(|_| SessionId::new()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn ids_round_trip_through_strings() {
        let id = TurnId::new();
        assert_eq!(id.to_string().parse::<TurnId>().unwrap(), id);
    }

    #[test]
    fn serde_is_a_plain_string() {
        let id = SessionId::new();
        let j = serde_json::to_string(&id).unwrap();
        assert_eq!(
            j,
            format!("\"{id}\""),
            "the wire form must stay a bare string"
        );
        assert_eq!(serde_json::from_str::<SessionId>(&j).unwrap(), id);
    }

    #[test]
    fn a_malformed_id_is_a_typed_error() {
        let err = "not-a-uuid".parse::<SessionId>().unwrap_err();
        assert_eq!(err.kind, "SessionId");
        assert!(err.to_string().contains("SessionId"));
    }

    /// Distinct types, so passing a turn id where a session id belongs will not compile.
    /// This test documents the property; the compiler enforces it.
    #[test]
    fn different_id_kinds_are_different_types() {
        let s = SessionId::new();
        let t = TurnId::from_uuid(*s.as_uuid());
        assert_eq!(s.to_string(), t.to_string(), "same bytes…");
        // …but `fn f(_: SessionId)` cannot be called with `t`.
    }

    /// Provider-issued call ids are not UUIDs and must survive verbatim.
    #[test]
    fn call_ids_keep_provider_formatting() {
        for raw in ["call_abc123", "toolu_01A9FG", "fc_1", "tool_0"] {
            let c = CallId::new(raw);
            assert_eq!(c.to_string(), raw);
            assert_eq!(serde_json::to_string(&c).unwrap(), format!("\"{raw}\""));
        }
    }
}
