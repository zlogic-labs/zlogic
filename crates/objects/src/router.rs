//! Dispatching an [`ObjectId`] to the store that can serve it.
//! # Why routing is by algorithm and not by a `kind` field on the id
//! The tempting alternative is to put the *location* in the id — `file:abc`, `remote:abc`. It
//! breaks two things:
//! 1. **`FileObjectStore`, `MemoryObjectStore` and `RemoteObjectStore` all use SHA-256**, so the
//!    same content has the same id in all three. That equality is exactly what makes a local
//!    cache in front of a remote store work. With `kind` in the id, `file:abc` and `remote:abc`
//!    would be different ids for identical bytes and the cache could never hit.
//! 2. **Location changes over time.** Cache an object and it is now in both places; migrate a
//!    workspace to a server and everything moves. An identity that changes when the bytes did
//!    not is not an identity.
//! Location is a deployment fact; the hash is the identity. And the algorithm happens to carry
//! *better* routing information than a location would: `git-sha1` can only be served by a git
//! store, while `sha256` can be served by any of the others — so this router is total without
//! anything extra in the id.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use crate::{HashAlgo, ObjectError, ObjectId, ObjectStore, Result};

/// Routes reads by algorithm and writes to a designated primary.
pub struct StoreRouter {
    /// Serves `sha256` ids.
    content: Arc<dyn ObjectStore>,
    /// Serves `git-sha1` ids. Absent when this deployment has no git store (a `virtual`
    /// workspace, for instance).
    git: Option<Arc<dyn ObjectStore>>,
}

impl StoreRouter {
    pub fn new(content: Arc<dyn ObjectStore>) -> Self {
        debug_assert_eq!(content.algo(), HashAlgo::Sha256);
        Self { content, git: None }
    }

    pub fn with_git(mut self, git: Arc<dyn ObjectStore>) -> Self {
        debug_assert_eq!(git.algo(), HashAlgo::GitSha1);
        self.git = Some(git);
        self
    }

    /// The store that can serve this id.
    /// A `git-sha1` id with no git store configured is an error rather than a miss: the object
    /// may well exist, we simply have nowhere to look. Reporting it as "not found" would send
    /// whoever is debugging in the wrong direction.
    pub fn route(&self, id: &ObjectId) -> Result<&Arc<dyn ObjectStore>> {
        match id.algo {
            HashAlgo::Sha256 => Ok(&self.content),
            HashAlgo::GitSha1 => self.git.as_ref().ok_or_else(|| {
                ObjectError::Backend(format!(
                    "{id} needs a git object store, but none is configured"
                ))
            }),
        }
    }

    /// Writes go to the content store. Trees are written through [`crate::GitObjectStore`]
    /// directly: they need an object type and a child list, neither of which this interface can
    /// express.
    pub fn put(&self, bytes: &[u8]) -> Result<ObjectId> {
        self.content.put(bytes)
    }

    pub fn put_path(&self, path: &Path) -> Result<ObjectId> {
        self.content.put_path(path)
    }

    pub fn open(&self, id: &ObjectId) -> Result<Box<dyn Read + Send>> {
        self.route(id)?.open(id)
    }

    pub fn get(&self, id: &ObjectId) -> Result<Vec<u8>> {
        self.route(id)?.get(id)
    }

    pub fn exists(&self, id: &ObjectId) -> Result<bool> {
        self.route(id)?.exists(id)
    }

    pub fn size(&self, id: &ObjectId) -> Result<u64> {
        self.route(id)?.size(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GitObjectStore, MemoryObjectStore};

    #[test]
    fn routes_by_algorithm() {
        let tmp = tempfile::tempdir().unwrap();
        let content = Arc::new(MemoryObjectStore::new());
        let git = Arc::new(GitObjectStore::open(tmp.path()).unwrap());
        let router = StoreRouter::new(content.clone()).with_git(git.clone());

        let a = content.put(b"content-addressed").unwrap();
        let b = git.put(b"git blob").unwrap();

        assert_eq!(router.get(&a).unwrap(), b"content-addressed");
        assert_eq!(router.get(&b).unwrap(), b"git blob");
    }

    /// The property that a `kind` in the id would have destroyed.
    #[test]
    fn the_same_bytes_have_one_id_across_every_sha256_store() {
        let dir = tempfile::tempdir().unwrap();
        let disk = crate::FileObjectStore::open(dir.path()).unwrap();
        let mem = MemoryObjectStore::new();

        let from_disk = disk.put(b"identical").unwrap();
        let from_mem = mem.put(b"identical").unwrap();
        assert_eq!(
            from_disk, from_mem,
            "a cache in front of a remote depends on this"
        );

        // Which means an id written by one store is readable through a router pointed at
        // another — migrating storage does not invalidate history.
        let router = StoreRouter::new(Arc::new(mem));
        assert_eq!(router.get(&from_disk).unwrap(), b"identical");
    }

    /// "No git store configured" must not look like "the object is gone".
    #[test]
    fn a_git_id_without_a_git_store_is_a_configuration_error() {
        let router = StoreRouter::new(Arc::new(MemoryObjectStore::new()));
        let git_id: ObjectId = "git-sha1:e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
            .parse()
            .unwrap();

        // `Box<dyn Read>` is not Debug, so match on the error rather than the whole Result.
        match router.open(&git_id) {
            Err(ObjectError::Backend(m)) => assert!(m.contains("git object store"), "{m}"),
            Err(other) => panic!("expected a configuration error, got {other}"),
            Ok(_) => panic!("expected a configuration error, got a reader"),
        }
    }

    #[test]
    fn writes_go_to_the_content_store() {
        let content = Arc::new(MemoryObjectStore::new());
        let router = StoreRouter::new(content.clone());
        let id = router.put(b"x").unwrap();
        assert_eq!(id.algo, HashAlgo::Sha256);
        assert!(content.exists(&id).unwrap());
    }

    #[test]
    fn missing_objects_still_report_not_found() {
        let router = StoreRouter::new(Arc::new(MemoryObjectStore::new()));
        let id: ObjectId =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .parse()
                .unwrap();
        assert!(matches!(router.open(&id), Err(ObjectError::NotFound(_))));
        assert!(!router.exists(&id).unwrap());
    }
}
