use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{HashAlgo, ObjectEntry, ObjectError, ObjectId, ObjectStore, Result, hex_of};

pub struct FileObjectStore {
    root: PathBuf,
}

impl FileObjectStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("tmp"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_of(&self, id: &ObjectId) -> PathBuf {
        let (a, b) = id.shard();
        self.root.join(a).join(b)
    }

    fn commit_temp(&self, tmp: &Path, id: &ObjectId) -> Result<()> {
        let dest = self.path_of(id);
        if dest.exists() {
            let _ = fs::remove_file(tmp);
            return Ok(());
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(tmp, &dest)?;
        Ok(())
    }

    fn temp_path(&self) -> PathBuf {
        self.root.join("tmp").join(crate::memory::unique_name())
    }
}

impl ObjectStore for FileObjectStore {
    fn algo(&self) -> HashAlgo {
        HashAlgo::Sha256
    }

    fn put(&self, bytes: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::new(HashAlgo::Sha256, hex_of(&Sha256::digest(bytes)));
        if self.path_of(&id).exists() {
            return Ok(id);
        }
        let tmp = self.temp_path();
        {
            let mut f = File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        self.commit_temp(&tmp, &id)?;
        Ok(id)
    }

    fn put_path(&self, path: &Path) -> Result<ObjectId> {
        let mut src = BufReader::new(File::open(path)?);
        let tmp = self.temp_path();
        let mut hasher = Sha256::new();
        {
            let mut out = File::create(&tmp)?;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = src.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n])?;
            }
            out.sync_all()?;
        }
        let id = ObjectId::new(HashAlgo::Sha256, hex_of(&hasher.finalize()));
        self.commit_temp(&tmp, &id)?;
        Ok(id)
    }

    fn open(&self, id: &ObjectId) -> Result<Box<dyn Read + Send>> {
        self.check_algo(id)?;
        match File::open(self.path_of(id)) {
            Ok(f) => Ok(Box::new(BufReader::new(f))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(ObjectError::NotFound(id.clone()))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn exists(&self, id: &ObjectId) -> Result<bool> {
        self.check_algo(id)?;
        Ok(self.path_of(id).exists())
    }

    fn scan(&self, visit: &mut dyn FnMut(ObjectEntry) -> Result<()>) -> Result<()> {
        let shards = match std::fs::read_dir(&self.root) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for shard in shards {
            let shard = shard?;
            let name = shard.file_name().to_string_lossy().into_owned();
            if name == "tmp" || !shard.file_type()?.is_dir() {
                continue;
            }
            for obj in std::fs::read_dir(shard.path())? {
                let obj = obj?;
                let meta = obj.metadata()?;
                if !meta.is_file() {
                    continue;
                }
                let hex = format!("{name}{}", obj.file_name().to_string_lossy());
                let Ok(id) = format!("{}:{hex}", HashAlgo::Sha256).parse::<ObjectId>() else {
                    continue;
                };
                visit(ObjectEntry {
                    id,
                    size: meta.len(),
                    stored_at: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                })?;
            }
        }
        Ok(())
    }

    fn delete(&self, id: &ObjectId) -> Result<bool> {
        self.check_algo(id)?;
        match std::fs::remove_file(self.path_of(id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn size(&self, id: &ObjectId) -> Result<u64> {
        self.check_algo(id)?;
        match fs::metadata(self.path_of(id)) {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(ObjectError::NotFound(id.clone()))
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, FileObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = FileObjectStore::open(dir.path()).unwrap();
        (dir, s)
    }

    #[test]
    fn put_then_get_round_trips() {
        let (_d, s) = store();
        let id = s.put(b"hello").unwrap();
        assert_eq!(s.get(&id).unwrap(), b"hello");
        assert_eq!(s.size(&id).unwrap(), 5);
        assert!(s.exists(&id).unwrap());
    }

    #[test]
    fn hashes_raw_content_without_a_header() {
        let (_d, s) = store();
        let id = s.put(b"").unwrap();
        assert_eq!(
            id.to_string(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn identical_content_dedups_to_one_object() {
        let (_d, s) = store();
        let a = s.put(b"same").unwrap();
        let b = s.put(b"same").unwrap();
        assert_eq!(a, b);
        let count = walkdir_count(s.root());
        assert_eq!(count, 1, "one file per distinct content");
    }

    #[test]
    fn put_path_matches_put_bytes() {
        let (_d, s) = store();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("big.bin");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::File::create(&f).unwrap().write_all(&data).unwrap();

        let from_path = s.put_path(&f).unwrap();
        let from_bytes = s.put(&data).unwrap();
        assert_eq!(
            from_path, from_bytes,
            "streaming and whole-buffer writes must compute the same id"
        );
        assert_eq!(s.size(&from_path).unwrap(), data.len() as u64);
    }

    #[test]
    fn open_streams_without_loading_everything() {
        let (_d, s) = store();
        let data = vec![7u8; 500_000];
        let id = s.put(&data).unwrap();
        let mut r = s.open(&id).unwrap();
        let mut head = [0u8; 16];
        r.read_exact(&mut head).unwrap();
        assert_eq!(
            head, [7u8; 16],
            "reading only the first 16 bytes must also succeed"
        );
    }

    #[test]
    fn missing_object_is_not_found_not_io_error() {
        let (_d, s) = store();
        let id: ObjectId =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .parse()
                .unwrap();
        assert!(matches!(s.open(&id), Err(ObjectError::NotFound(_))));
        assert!(!s.exists(&id).unwrap());
    }

    #[test]
    fn wrong_algorithm_fails_loudly() {
        let (_d, s) = store();
        let git: ObjectId = "git-sha1:e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
            .parse()
            .unwrap();
        assert!(matches!(
            s.open(&git),
            Err(ObjectError::AlgoMismatch { .. })
        ));
        assert!(matches!(
            s.exists(&git),
            Err(ObjectError::AlgoMismatch { .. })
        ));
    }

    #[test]
    fn no_temp_files_are_left_behind() {
        let (_d, s) = store();
        s.put(b"a").unwrap();
        s.put(b"a").unwrap(); // the second call takes the "already exists" branch and must leave no litter
        let tmp: Vec<_> = std::fs::read_dir(s.root().join("tmp")).unwrap().collect();
        assert!(tmp.is_empty(), "the temp directory must be empty");
    }

    fn walkdir_count(root: &Path) -> usize {
        let mut n = 0;
        for shard in std::fs::read_dir(root).unwrap().flatten() {
            if shard.file_name() == "tmp" {
                continue;
            }
            if shard.path().is_dir() {
                n += std::fs::read_dir(shard.path()).unwrap().count();
            }
        }
        n
    }
}
