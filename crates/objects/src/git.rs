//! ```text
//! ```

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use git2::{ObjectType, Odb, Repository};

use crate::{HashAlgo, ObjectError, ObjectId, ObjectStore, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitKind {
    Blob,
    Tree,
    Commit,
}

impl GitKind {
    fn from_git2(t: ObjectType) -> Option<Self> {
        match t {
            ObjectType::Blob => Some(GitKind::Blob),
            ObjectType::Tree => Some(GitKind::Tree),
            ObjectType::Commit => Some(GitKind::Commit),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeMode {
    File,
    Exec,
    Dir,
    Symlink,
}

impl TreeMode {
    pub fn as_git(self) -> i32 {
        match self {
            TreeMode::File => 0o100644,
            TreeMode::Exec => 0o100755,
            TreeMode::Dir => 0o040000,
            TreeMode::Symlink => 0o120000,
        }
    }

    pub fn from_git(mode: i32) -> Self {
        match mode {
            0o100755 => TreeMode::Exec,
            0o040000 => TreeMode::Dir,
            0o120000 => TreeMode::Symlink,
            _ => TreeMode::File,
        }
    }

    pub fn is_dir(self) -> bool {
        matches!(self, TreeMode::Dir)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TreeEntry {
    pub mode: TreeMode,
    pub name: String,
    pub id: ObjectId,
}

impl TreeEntry {
    pub fn file(name: impl Into<String>, id: ObjectId) -> Self {
        Self {
            mode: TreeMode::File,
            name: name.into(),
            id,
        }
    }

    pub fn exec(name: impl Into<String>, id: ObjectId) -> Self {
        Self {
            mode: TreeMode::Exec,
            name: name.into(),
            id,
        }
    }

    pub fn dir(name: impl Into<String>, id: ObjectId) -> Self {
        Self {
            mode: TreeMode::Dir,
            name: name.into(),
            id,
        }
    }

    pub fn symlink(name: impl Into<String>, id: ObjectId) -> Self {
        Self {
            mode: TreeMode::Symlink,
            name: name.into(),
            id,
        }
    }
}

pub struct GitObjectStore {
    root: PathBuf,
    repo: Mutex<Repository>,
}

impl GitObjectStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let repo = match Repository::open_bare(&root) {
            Ok(r) => r,
            Err(_) => Repository::init_bare(&root).map_err(backend)?,
        };
        Ok(Self {
            root,
            repo: Mutex::new(repo),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_of(&self, id: &ObjectId) -> PathBuf {
        let (a, b) = id.shard();
        self.root.join("objects").join(a).join(b)
    }

    fn repo(&self) -> MutexGuard<'_, Repository> {
        self.repo.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn oid(&self, id: &ObjectId) -> Result<git2::Oid> {
        self.check_algo(id)?;
        git2::Oid::from_str(&id.hex).map_err(|_| ObjectError::BadId(id.hex.clone()))
    }

    fn write(&self, kind: ObjectType, payload: &[u8]) -> Result<ObjectId> {
        let repo = self.repo();
        let odb = repo.odb().map_err(backend)?;
        let oid = odb.write(kind, payload).map_err(backend)?;
        Ok(ObjectId::new(HashAlgo::GitSha1, oid.to_string()))
    }

    pub fn write_tree(&self, entries: &[TreeEntry]) -> Result<ObjectId> {
        let repo = self.repo();
        let mut builder = repo.treebuilder(None).map_err(backend)?;
        for e in entries {
            if e.id.algo != HashAlgo::GitSha1 {
                return Err(ObjectError::AlgoMismatch {
                    store: HashAlgo::GitSha1,
                    id: e.id.algo,
                });
            }
            let oid =
                git2::Oid::from_str(&e.id.hex).map_err(|_| ObjectError::BadId(e.id.hex.clone()))?;
            builder
                .insert(&e.name, oid, e.mode.as_git())
                .map_err(backend)?;
        }
        let oid = builder.write().map_err(backend)?;
        Ok(ObjectId::new(HashAlgo::GitSha1, oid.to_string()))
    }

    pub fn blob_line_stats(
        &self,
        before: Option<&ObjectId>,
        after: Option<&ObjectId>,
    ) -> Result<Option<(u32, u32)>> {
        let repo = self.repo();
        let load = |id: Option<&ObjectId>| -> Result<Option<git2::Blob<'_>>> {
            match id {
                None => Ok(None),
                Some(id) => {
                    self.check_algo(id)?;
                    let oid = git2::Oid::from_str(&id.hex)
                        .map_err(|_| ObjectError::BadId(id.hex.clone()))?;
                    Ok(Some(repo.find_blob(oid).map_err(|e| match e.code() {
                        git2::ErrorCode::NotFound => ObjectError::NotFound(id.clone()),
                        _ => backend(e),
                    })?))
                }
            }
        };
        let (old, new) = (load(before)?, load(after)?);

        let mut added = 0u32;
        let mut removed = 0u32;
        let mut binary = false;
        repo.diff_blobs(
            old.as_ref(),
            Some("f"),
            new.as_ref(),
            Some("f"),
            None,
            None,
            Some(&mut |_delta, _binary| {
                binary = true;
                true
            }),
            None,
            Some(&mut |_delta, _hunk, line| {
                match line.origin() {
                    '+' => added += 1,
                    '-' => removed += 1,
                    _ => {}
                }
                true
            }),
        )
        .map_err(backend)?;

        Ok((!binary).then_some((added, removed)))
    }

    pub fn unified_diff(
        &self,
        before: Option<&ObjectId>,
        after: Option<&ObjectId>,
        path: &str,
    ) -> Result<Option<String>> {
        let repo = self.repo();
        let load = |id: Option<&ObjectId>| -> Result<Option<git2::Blob<'_>>> {
            match id {
                None => Ok(None),
                Some(id) => {
                    self.check_algo(id)?;
                    let oid = git2::Oid::from_str(&id.hex)
                        .map_err(|_| ObjectError::BadId(id.hex.clone()))?;
                    Ok(Some(repo.find_blob(oid).map_err(|e| match e.code() {
                        git2::ErrorCode::NotFound => ObjectError::NotFound(id.clone()),
                        _ => backend(e),
                    })?))
                }
            }
        };
        let (old, new) = (load(before)?, load(after)?);

        let body = std::cell::RefCell::new(String::new());
        let mut binary = false;
        repo.diff_blobs(
            old.as_ref(),
            Some(path),
            new.as_ref(),
            Some(path),
            None,
            None,
            Some(&mut |_delta, _binary| {
                binary = true;
                true
            }),
            Some(&mut |_delta, hunk| {
                body.borrow_mut()
                    .push_str(&String::from_utf8_lossy(hunk.header()));
                true
            }),
            Some(&mut |_delta, _hunk, line| {
                let mut body = body.borrow_mut();
                body.push(line.origin());
                body.push_str(&String::from_utf8_lossy(line.content()));
                true
            }),
        )
        .map_err(backend)?;

        let body = body.into_inner();
        if binary {
            return Ok(None);
        }
        if body.is_empty() {
            return Ok(Some(String::new()));
        }
        let a = if old.is_some() {
            format!("a/{path}")
        } else {
            "/dev/null".into()
        };
        let b = if new.is_some() {
            format!("b/{path}")
        } else {
            "/dev/null".into()
        };
        Ok(Some(format!("--- {a}\n+++ {b}\n{body}")))
    }

    pub fn read_tree(&self, id: &ObjectId) -> Result<Vec<TreeEntry>> {
        let oid = self.oid(id)?;
        let repo = self.repo();
        let tree = repo.find_tree(oid).map_err(|e| match e.code() {
            git2::ErrorCode::NotFound => ObjectError::NotFound(id.clone()),
            _ => backend(e),
        })?;
        Ok(tree
            .iter()
            .map(|e| TreeEntry {
                mode: TreeMode::from_git(e.filemode()),
                name: e.name().unwrap_or_default().to_string(),
                id: ObjectId::new(HashAlgo::GitSha1, e.id().to_string()),
            })
            .collect())
    }

    pub fn lookup_path(&self, tree: &ObjectId, path: &str) -> Result<Option<(ObjectId, TreeMode)>> {
        if path.is_empty() || path.starts_with('/') {
            return Ok(None);
        }
        let mut parts = path.split('/').peekable();
        let mut current = tree.clone();
        while let Some(part) = parts.next() {
            if part.is_empty() || part == "." || part == ".." {
                return Ok(None);
            }
            let entries = self.read_tree(&current)?;
            let Some(entry) = entries.into_iter().find(|e| e.name == part) else {
                return Ok(None);
            };
            if parts.peek().is_none() {
                return Ok(Some((entry.id, entry.mode)));
            }
            if !entry.mode.is_dir() {
                return Ok(None);
            }
            current = entry.id;
        }
        unreachable!(
            "a non-empty path split('/') always has at least one segment, so the loop returns inside"
        )
    }

    pub fn stat(&self, id: &ObjectId) -> Result<(GitKind, u64)> {
        let oid = self.oid(id)?;
        let repo = self.repo();
        let odb = repo.odb().map_err(backend)?;
        let (len, kind) = odb.read_header(oid).map_err(|e| not_found(e, id))?;
        let kind = GitKind::from_git2(kind).ok_or_else(|| ObjectError::Corrupt {
            id: id.clone(),
            reason: format!("unsupported object kind: {kind:?}"),
        })?;
        Ok((kind, len as u64))
    }

    pub fn for_each_id(&self, mut f: impl FnMut(ObjectId)) -> Result<()> {
        let repo = self.repo();
        let odb = repo.odb().map_err(backend)?;
        odb.foreach(|oid| {
            f(ObjectId::new(HashAlgo::GitSha1, oid.to_string()));
            true
        })
        .map_err(backend)?;
        Ok(())
    }

    pub fn remove(&self, id: &ObjectId) -> Result<bool> {
        self.check_algo(id)?;
        let path = self.path_of(id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

impl ObjectStore for GitObjectStore {
    fn algo(&self) -> HashAlgo {
        HashAlgo::GitSha1
    }

    fn put(&self, bytes: &[u8]) -> Result<ObjectId> {
        self.write(ObjectType::Blob, bytes)
    }

    fn put_path(&self, path: &Path) -> Result<ObjectId> {
        let len = std::fs::metadata(path)?.len();
        let mut src = std::io::BufReader::new(std::fs::File::open(path)?);

        let repo = self.repo();
        let odb = repo.odb().map_err(backend)?;
        let mut writer = odb
            .writer(len as usize, ObjectType::Blob)
            .map_err(backend)?;

        let mut buf = vec![0u8; 64 * 1024];
        let mut seen = 0u64;
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            seen += n as u64;
            writer.write_all(&buf[..n])?;
        }
        if seen != len {
            return Err(ObjectError::Backend(format!(
                "file changed while hashing: {} (expected {len} bytes, read {seen})",
                path.display()
            )));
        }
        let oid = writer.finalize().map_err(backend)?;
        Ok(ObjectId::new(HashAlgo::GitSha1, oid.to_string()))
    }

    fn open(&self, id: &ObjectId) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(std::io::Cursor::new(self.get(id)?)))
    }

    fn get(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let oid = self.oid(id)?;
        let repo = self.repo();
        let odb = repo.odb().map_err(backend)?;
        let obj = odb.read(oid).map_err(|e| not_found(e, id))?;
        Ok(obj.data().to_vec())
    }

    fn exists(&self, id: &ObjectId) -> Result<bool> {
        let oid = self.oid(id)?;
        let repo = self.repo();
        let odb: Odb<'_> = repo.odb().map_err(backend)?;
        Ok(odb.exists(oid))
    }

    fn size(&self, id: &ObjectId) -> Result<u64> {
        Ok(self.stat(id)?.1)
    }
}

fn backend(e: git2::Error) -> ObjectError {
    ObjectError::Backend(e.to_string())
}

fn not_found(e: git2::Error, id: &ObjectId) -> ObjectError {
    if e.code() == git2::ErrorCode::NotFound {
        ObjectError::NotFound(id.clone())
    } else {
        backend(e)
    }
}

#[cfg(test)]
mod diff_tests {
    use super::*;

    fn store() -> (tempfile::TempDir, GitObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = GitObjectStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn a_modification_reads_like_git_diff() {
        let (_dir, store) = store();
        let before = store.put(b"one\ntwo\nthree\n").unwrap();
        let after = store.put(b"one\nTWO\nthree\n").unwrap();

        let patch = store
            .unified_diff(Some(&before), Some(&after), "src/main.rs")
            .unwrap()
            .expect("text content must produce a diff");

        assert!(patch.contains("--- a/src/main.rs"), "{patch}");
        assert!(patch.contains("+++ b/src/main.rs"), "{patch}");
        assert!(patch.contains("@@"), "there must be a hunk header: {patch}");
        assert!(patch.contains("-two\n"), "{patch}");
        assert!(patch.contains("+TWO\n"), "{patch}");
        assert!(patch.contains(" one\n"), "{patch}");
    }

    #[test]
    fn a_new_file_shows_every_line_as_added() {
        let (_dir, store) = store();
        let after = store.put(b"hello\n").unwrap();

        let patch = store
            .unified_diff(None, Some(&after), "new.txt")
            .unwrap()
            .unwrap();
        assert!(patch.contains("+hello\n"), "{patch}");
        assert!(!patch.contains("-hello"), "{patch}");

        let patch = store
            .unified_diff(Some(&after), None, "gone.txt")
            .unwrap()
            .unwrap();
        assert!(patch.contains("-hello\n"), "{patch}");
    }

    #[test]
    fn binary_content_has_no_line_diff() {
        let (_dir, store) = store();
        let before = store.put(&[0u8, 1, 2, 3, 0, 5]).unwrap();
        let after = store.put(&[0u8, 9, 9, 9, 0, 5]).unwrap();

        assert_eq!(
            store
                .unified_diff(Some(&before), Some(&after), "a.bin")
                .unwrap(),
            None
        );
    }

    #[test]
    fn identical_content_yields_an_empty_patch() {
        let (_dir, store) = store();
        let id = store.put(b"same\n").unwrap();
        assert_eq!(
            store
                .unified_diff(Some(&id), Some(&id), "a.txt")
                .unwrap()
                .as_deref(),
            Some("")
        );
    }

    #[test]
    fn nothing_on_either_side_is_empty_not_a_panic() {
        let (_dir, store) = store();
        assert_eq!(
            store.unified_diff(None, None, "a.txt").unwrap().as_deref(),
            Some("")
        );
    }

    #[test]
    fn a_missing_trailing_newline_is_marked() {
        let (_dir, store) = store();
        let before = store.put(b"no newline").unwrap();
        let after = store.put(b"no newline\n").unwrap();

        let patch = store
            .unified_diff(Some(&before), Some(&after), "a.txt")
            .unwrap()
            .unwrap();
        assert!(patch.contains("\\ No newline at end of file"), "{patch}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, GitObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = GitObjectStore::open(dir.path()).unwrap();
        (dir, s)
    }

    #[test]
    fn empty_blob_matches_gits_well_known_hash() {
        let (_d, s) = store();
        let id = s.put(b"").unwrap();
        assert_eq!(
            id.to_string(),
            "git-sha1:e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }

    #[test]
    fn blob_hash_matches_git_hash_object() {
        let (_d, s) = store();
        let id = s.put(b"hello").unwrap();
        assert_eq!(
            id.to_string(),
            "git-sha1:b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0"
        );
    }

    #[test]
    fn round_trips_content() {
        let (_d, s) = store();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        let id = s.put(&data).unwrap();
        assert_eq!(s.get(&id).unwrap(), data);
        assert_eq!(
            s.size(&id).unwrap(),
            data.len() as u64,
            "size is the uncompressed length"
        );
        assert_eq!(s.stat(&id).unwrap().0, GitKind::Blob);
        assert!(s.exists(&id).unwrap());
    }

    #[test]
    fn objects_land_as_loose_files_where_gc_can_find_them() {
        let (_d, s) = store();
        let id = s.put(b"hello").unwrap();
        assert!(
            s.path_of(&id).is_file(),
            "{} must exist",
            s.path_of(&id).display()
        );

        assert!(s.remove(&id).unwrap());
        assert!(!s.exists(&id).unwrap());
        assert!(!s.remove(&id).unwrap(), "removing twice is not an error");
    }

    #[test]
    fn put_path_matches_put_bytes_and_streams() {
        let (_d, s) = store();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.bin");
        let data: Vec<u8> = (0..150_000u32).map(|i| (i % 97) as u8).collect();
        std::fs::write(&f, &data).unwrap();
        assert_eq!(s.put_path(&f).unwrap(), s.put(&data).unwrap());
    }

    #[test]
    fn content_is_stored_byte_for_byte_without_filters() {
        let (_d, s) = store();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitattributes"), "* text=auto eol=lf\n").unwrap();
        let f = dir.path().join("crlf.txt");
        let raw = b"line one\r\nline two\r\n";
        std::fs::write(&f, raw).unwrap();

        let id = s.put_path(&f).unwrap();
        assert_eq!(s.get(&id).unwrap(), raw, "CRLF must be preserved verbatim");
        assert_eq!(
            id,
            s.put(raw).unwrap(),
            "put_path and put must yield the same id"
        );
    }

    #[test]
    fn open_reads_past_the_header() {
        let (_d, s) = store();
        let id = s.put(&vec![9u8; 400_000]).unwrap();
        let mut r = s.open(&id).unwrap();
        let mut head = [0u8; 8];
        r.read_exact(&mut head).unwrap();
        assert_eq!(head, [9u8; 8], "the first byte read is the payload");
    }

    #[test]
    fn tree_entries_are_sorted_the_way_git_wants() {
        let (_d, s) = store();
        let blob = s.put(b"x").unwrap();
        let sub = s.write_tree(&[TreeEntry::file("a", blob.clone())]).unwrap();

        let t = s
            .write_tree(&[
                TreeEntry::dir("sub", sub.clone()),
                TreeEntry::file("sub.txt", blob.clone()),
                TreeEntry::file("a.txt", blob.clone()),
            ])
            .unwrap();

        let names: Vec<String> = s
            .read_tree(&t)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            ["a.txt", "sub.txt", "sub"],
            "the order must be a.txt < sub.txt < sub/"
        );
    }

    #[test]
    fn a_tree_round_trips_modes_and_children() {
        let (_d, s) = store();
        let blob = s.put(b"#!/bin/sh\n").unwrap();
        let link = s.put(b"../target").unwrap();
        let sub = s
            .write_tree(&[TreeEntry::file("inner", blob.clone())])
            .unwrap();

        let t = s
            .write_tree(&[
                TreeEntry::exec("run.sh", blob.clone()),
                TreeEntry::symlink("here", link.clone()),
                TreeEntry::dir("d", sub.clone()),
            ])
            .unwrap();

        let entries = s.read_tree(&t).unwrap();
        let by_name = |n: &str| entries.iter().find(|e| e.name == n).unwrap().clone();
        assert_eq!(by_name("run.sh").mode, TreeMode::Exec);
        assert_eq!(by_name("here").mode, TreeMode::Symlink);
        assert_eq!(by_name("d").mode, TreeMode::Dir);
        assert_eq!(by_name("d").id, sub, "the subtree id must be unchanged");
        assert_eq!(s.stat(&t).unwrap().0, GitKind::Tree);
    }

    #[test]
    fn identical_trees_have_identical_ids() {
        let (_d, s) = store();
        let blob = s.put(b"same").unwrap();
        let a = s.write_tree(&[TreeEntry::file("f", blob.clone())]).unwrap();
        let b = s.write_tree(&[TreeEntry::file("f", blob)]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn tree_rejects_foreign_object_ids() {
        let (_d, s) = store();
        let alien = ObjectId::new(
            HashAlgo::Sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert!(matches!(
            s.write_tree(&[TreeEntry::file("a", alien)]),
            Err(ObjectError::AlgoMismatch { .. })
        ));
    }

    #[test]
    fn every_object_can_be_enumerated() {
        let (_d, s) = store();
        let a = s.put(b"one").unwrap();
        let b = s.put(b"two").unwrap();
        let t = s.write_tree(&[TreeEntry::file("a", a.clone())]).unwrap();

        let mut seen = Vec::new();
        s.for_each_id(|id| seen.push(id)).unwrap();
        for want in [&a, &b, &t] {
            assert!(seen.contains(want), "{want} was not enumerated");
        }
    }

    #[test]
    fn wrong_algorithm_fails_loudly() {
        let (_d, s) = store();
        let sha: ObjectId =
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .parse()
                .unwrap();
        assert!(matches!(
            s.open(&sha),
            Err(ObjectError::AlgoMismatch { .. })
        ));
    }

    #[test]
    fn missing_object_is_not_found() {
        let (_d, s) = store();
        let id: ObjectId = "git-sha1:0000000000000000000000000000000000000000"
            .parse()
            .unwrap();
        assert!(matches!(s.open(&id), Err(ObjectError::NotFound(_))));
        assert!(matches!(s.read_tree(&id), Err(ObjectError::NotFound(_))));
        assert!(!s.exists(&id).unwrap());
    }

    #[test]
    fn a_reopened_store_sees_earlier_objects() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let s = GitObjectStore::open(dir.path()).unwrap();
            s.put(b"persisted").unwrap()
        };
        let s = GitObjectStore::open(dir.path()).unwrap();
        assert_eq!(s.get(&id).unwrap(), b"persisted");
    }

    fn nested_tree(s: &GitObjectStore) -> ObjectId {
        let blob = s.put(b"x\n").unwrap();
        let sub = s
            .write_tree(&[
                TreeEntry::file("inner.txt", blob.clone()),
                TreeEntry::exec("run.sh", blob.clone()),
            ])
            .unwrap();
        s.write_tree(&[
            TreeEntry::dir("d", sub.clone()),
            TreeEntry::file("root.txt", blob),
        ])
        .unwrap()
    }

    #[test]
    fn lookup_path_finds_a_file_through_dirs() {
        let (_d, s) = store();
        let t = nested_tree(&s);

        let (id, mode) = s.lookup_path(&t, "d/run.sh").unwrap().unwrap();
        assert_eq!(mode, TreeMode::Exec);
        assert_eq!(s.get(&id).unwrap(), b"x\n");

        let (_, mode) = s.lookup_path(&t, "root.txt").unwrap().unwrap();
        assert_eq!(mode, TreeMode::File);
    }

    #[test]
    fn lookup_path_hits_a_directory_itself() {
        let (_d, s) = store();
        let t = nested_tree(&s);

        let (sub, mode) = s.lookup_path(&t, "d").unwrap().unwrap();
        assert_eq!(mode, TreeMode::Dir);
        let names: Vec<String> = s
            .read_tree(&sub)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["inner.txt", "run.sh"]);
    }

    #[test]
    fn lookup_path_misses_are_none_not_errors() {
        let (_d, s) = store();
        let t = nested_tree(&s);

        for p in [
            "missing.txt",
            "d/missing.txt",
            "no/such/dir/f.txt",
            "",
            "/absolute.txt",
            "d/run.sh/extra", // the middle component is not a directory
            "a//b",
            "../escape",
            "./dot",
        ] {
            assert_eq!(s.lookup_path(&t, p).unwrap(), None, "path {p:?}");
        }
    }
}
