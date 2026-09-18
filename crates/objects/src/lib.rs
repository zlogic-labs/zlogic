//! # zlogic-objects

pub mod fs;
pub mod git;
pub mod memory;
pub mod remote;
pub mod repo_facts;
pub mod router;

use std::fmt;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub use fs::FileObjectStore;
pub use git::{GitKind, GitObjectStore, TreeEntry, TreeMode};
pub use memory::MemoryObjectStore;
pub use remote::{BlobTransport, RemoteObjectStore};
pub use repo_facts::{RepoFacts, WorkTreeStatus};
pub use router::StoreRouter;

#[derive(Debug, thiserror::Error)]
pub enum ObjectError {
    #[error("object not found: {0}")]
    NotFound(ObjectId),
    #[error("object id algorithm mismatch: store is {store}, id is {id}")]
    AlgoMismatch { store: HashAlgo, id: HashAlgo },
    #[error("malformed object id: {0}")]
    BadId(String),
    #[error("corrupt object {id}: {reason}")]
    Corrupt { id: ObjectId, reason: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Backend(String),
    #[error("not supported by this object store: {0}")]
    Unsupported(&'static str),
}

pub type Result<T> = std::result::Result<T, ObjectError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HashAlgo {
    Sha256,
    GitSha1,
}

impl HashAlgo {
    pub fn as_str(self) -> &'static str {
        match self {
            HashAlgo::Sha256 => "sha256",
            HashAlgo::GitSha1 => "git-sha1",
        }
    }

    pub fn hex_len(self) -> usize {
        match self {
            HashAlgo::Sha256 => 64,
            HashAlgo::GitSha1 => 40,
        }
    }
}

impl fmt::Display for HashAlgo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct ObjectId {
    pub algo: HashAlgo,
    pub hex: String,
}

impl ObjectId {
    pub fn new(algo: HashAlgo, hex: impl Into<String>) -> Self {
        Self {
            algo,
            hex: hex.into(),
        }
    }

    pub fn shard(&self) -> (&str, &str) {
        self.hex.split_at(2)
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algo, self.hex)
    }
}

impl From<ObjectId> for String {
    fn from(id: ObjectId) -> String {
        id.to_string()
    }
}

impl TryFrom<String> for ObjectId {
    type Error = ObjectError;
    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl FromStr for ObjectId {
    type Err = ObjectError;

    fn from_str(s: &str) -> Result<Self> {
        let (algo, hex) = s
            .split_once(':')
            .ok_or_else(|| ObjectError::BadId(format!("missing algorithm prefix: {s}")))?;
        let algo = match algo {
            "sha256" => HashAlgo::Sha256,
            "git-sha1" => HashAlgo::GitSha1,
            other => return Err(ObjectError::BadId(format!("unknown algorithm: {other}"))),
        };
        if hex.len() != algo.hex_len() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ObjectError::BadId(format!("bad hex for {algo}: {hex}")));
        }
        Ok(ObjectId {
            algo,
            hex: hex.to_ascii_lowercase(),
        })
    }
}

/// Why something references an object.
/// Lives here rather than in the store or the tools crate because both produce these and both
/// consume them: a tool declares "this object is my captured stdout", the store records it, core
/// carries it between them. Two definitions of a three-field struct is how a field gets added to
/// one and forgotten in the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectRole {
    /// The referring record's own body was offloaded into this object.
    /// **At most one per record** — it is what "read the payload back" looks for.
    Payload,
    /// Something the user attached.
    Attachment,
    /// Captured output from a tool.
    Output,
    /// A captured diff.
    Diff,
    /// The immutable body referenced by a lightweight loaded-skill state entry.
    Skill,
}

impl ObjectRole {
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectRole::Payload => "payload",
            ObjectRole::Attachment => "attachment",
            ObjectRole::Output => "output",
            ObjectRole::Diff => "diff",
            ObjectRole::Skill => "skill",
        }
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        Some(match s {
            "payload" => ObjectRole::Payload,
            "attachment" => ObjectRole::Attachment,
            "output" => ObjectRole::Output,
            "diff" => ObjectRole::Diff,
            "skill" => ObjectRole::Skill,
            _ => return None,
        })
    }

    pub const ALL: &'static [Self] = &[
        Self::Payload,
        Self::Attachment,
        Self::Output,
        Self::Diff,
        Self::Skill,
    ];
}

impl fmt::Display for ObjectRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(feature = "sql")]
impl rusqlite::ToSql for ObjectRole {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
    }
}

#[cfg(feature = "sql")]
impl rusqlite::types::FromSql for ObjectRole {
    fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let s = v.as_str()?;
        // Unknown values must be loud: silently defaulting a role would make a reference written
        // by a newer version look like ordinary data.
        Self::parse_str(s).ok_or_else(|| {
            rusqlite::types::FromSqlError::Other(Box::new(ObjectError::BadId(format!(
                "unknown object role: {s}"
            ))))
        })
    }
}

/// A reference to an object, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRef {
    pub object_id: ObjectId,
    pub role: ObjectRole,
    /// Ties the reference back to a specific place in the referring record — an attachment index,
    /// a file path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<String>,
}

impl ObjectRef {
    pub fn new(object_id: ObjectId, role: ObjectRole) -> Self {
        Self {
            object_id,
            role,
            ref_key: None,
            kind: None,
            label: None,
            meta: None,
        }
    }

    pub fn keyed(object_id: ObjectId, role: ObjectRole, ref_key: impl Into<String>) -> Self {
        Self {
            object_id,
            role,
            ref_key: Some(ref_key.into()),
            kind: None,
            label: None,
            meta: None,
        }
    }

    pub fn classified(
        object_id: ObjectId,
        role: ObjectRole,
        kind: impl Into<String>,
        label: Option<String>,
        meta: Option<String>,
    ) -> Self {
        Self {
            object_id,
            role,
            ref_key: None,
            kind: Some(kind.into()),
            label,
            meta,
        }
    }

    pub fn payload(object_id: ObjectId) -> Self {
        Self::new(object_id, ObjectRole::Payload)
    }

    pub fn output(object_id: ObjectId) -> Self {
        Self::new(object_id, ObjectRole::Output)
    }
}

pub trait ObjectStore: Send + Sync {
    fn algo(&self) -> HashAlgo;

    fn put(&self, bytes: &[u8]) -> Result<ObjectId>;

    fn put_path(&self, path: &Path) -> Result<ObjectId>;

    fn open(&self, id: &ObjectId) -> Result<Box<dyn Read + Send>>;

    fn exists(&self, id: &ObjectId) -> Result<bool>;

    fn size(&self, id: &ObjectId) -> Result<u64>;

    fn get(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.open(id)?.read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn check_algo(&self, id: &ObjectId) -> Result<()> {
        if id.algo != self.algo() {
            return Err(ObjectError::AlgoMismatch {
                store: self.algo(),
                id: id.algo,
            });
        }
        Ok(())
    }

    fn scan(&self, _visit: &mut dyn FnMut(ObjectEntry) -> Result<()>) -> Result<()> {
        Err(ObjectError::Unsupported("enumerating objects"))
    }

    fn delete(&self, _id: &ObjectId) -> Result<bool> {
        Err(ObjectError::Unsupported("deleting objects"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectEntry {
    pub id: ObjectId,
    pub size: u64,
    pub stored_at: std::time::SystemTime,
}

#[cfg(feature = "sql")]
impl rusqlite::ToSql for ObjectId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.to_string()))
    }
}

#[cfg(feature = "sql")]
impl rusqlite::types::FromSql for ObjectId {
    fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let s = v.as_str()?;
        s.parse()
            .map_err(|e: ObjectError| rusqlite::types::FromSqlError::Other(Box::new(e)))
    }
}

pub(crate) fn hex_of(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_round_trips_through_its_string_form() {
        let raw = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let id: ObjectId = raw.parse().unwrap();
        assert_eq!(id.algo, HashAlgo::Sha256);
        assert_eq!(id.to_string(), raw);
    }

    #[test]
    fn hex_length_is_checked_per_algorithm() {
        assert!(
            "git-sha1:e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
                .parse::<ObjectId>()
                .is_ok()
        );
        assert!(
            "sha256:e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
                .parse::<ObjectId>()
                .is_err()
        );
    }

    #[test]
    fn missing_or_unknown_prefix_is_rejected() {
        assert!(
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
                .parse::<ObjectId>()
                .is_err()
        );
        assert!("md5:abc".parse::<ObjectId>().is_err());
    }

    #[test]
    fn serde_uses_the_flat_string_form() {
        let id: ObjectId = "git-sha1:e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
            .parse()
            .unwrap();
        let j = serde_json::to_string(&id).unwrap();
        assert_eq!(j, "\"git-sha1:e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\"");
        assert_eq!(serde_json::from_str::<ObjectId>(&j).unwrap(), id);
    }
}
