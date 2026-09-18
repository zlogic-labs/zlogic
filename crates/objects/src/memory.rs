use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::{HashAlgo, ObjectEntry, ObjectError, ObjectId, ObjectStore, Result, hex_of};

struct Blob {
    bytes: Vec<u8>,
    stored_at: std::time::SystemTime,
}

#[derive(Default)]
pub struct MemoryObjectStore {
    blobs: RwLock<HashMap<String, Blob>>,
}

impl MemoryObjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.blobs.read().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ObjectStore for MemoryObjectStore {
    fn algo(&self) -> HashAlgo {
        HashAlgo::Sha256
    }

    fn put(&self, bytes: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::new(HashAlgo::Sha256, hex_of(&Sha256::digest(bytes)));
        if let Ok(mut m) = self.blobs.write() {
            m.entry(id.hex.clone()).or_insert_with(|| Blob {
                bytes: bytes.to_vec(),
                stored_at: std::time::SystemTime::now(),
            });
        }
        Ok(id)
    }

    fn put_path(&self, path: &Path) -> Result<ObjectId> {
        self.put(&std::fs::read(path)?)
    }

    fn open(&self, id: &ObjectId) -> Result<Box<dyn Read + Send>> {
        self.check_algo(id)?;
        let m = self
            .blobs
            .read()
            .map_err(|_| ObjectError::Backend("poisoned".into()))?;
        let data = m
            .get(&id.hex)
            .ok_or_else(|| ObjectError::NotFound(id.clone()))?
            .bytes
            .clone();
        Ok(Box::new(Cursor::new(data)))
    }

    fn exists(&self, id: &ObjectId) -> Result<bool> {
        self.check_algo(id)?;
        let m = self
            .blobs
            .read()
            .map_err(|_| ObjectError::Backend("poisoned".into()))?;
        Ok(m.contains_key(&id.hex))
    }

    fn size(&self, id: &ObjectId) -> Result<u64> {
        self.check_algo(id)?;
        let m = self
            .blobs
            .read()
            .map_err(|_| ObjectError::Backend("poisoned".into()))?;
        m.get(&id.hex)
            .map(|b| b.bytes.len() as u64)
            .ok_or_else(|| ObjectError::NotFound(id.clone()))
    }

    fn scan(&self, visit: &mut dyn FnMut(ObjectEntry) -> Result<()>) -> Result<()> {
        let snapshot: Vec<ObjectEntry> = {
            let m = self
                .blobs
                .read()
                .map_err(|_| ObjectError::Backend("poisoned".into()))?;
            m.iter()
                .map(|(hex, b)| ObjectEntry {
                    id: ObjectId::new(HashAlgo::Sha256, hex.clone()),
                    size: b.bytes.len() as u64,
                    stored_at: b.stored_at,
                })
                .collect()
        };
        for e in snapshot {
            visit(e)?;
        }
        Ok(())
    }

    fn delete(&self, id: &ObjectId) -> Result<bool> {
        self.check_algo(id)?;
        let mut m = self
            .blobs
            .write()
            .map_err(|_| ObjectError::Backend("poisoned".into()))?;
        Ok(m.remove(&id.hex).is_some())
    }
}

pub(crate) fn unique_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("tmp-{}-{}-{}", std::process::id(), nanos, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn behaves_like_the_file_store() {
        let s = MemoryObjectStore::new();
        let id = s.put(b"hello").unwrap();
        assert_eq!(s.get(&id).unwrap(), b"hello");
        assert_eq!(s.size(&id).unwrap(), 5);
        assert!(s.exists(&id).unwrap());
        assert_eq!(s.put(b"hello").unwrap(), id);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn ids_match_the_file_store_for_the_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let disk = crate::FileObjectStore::open(dir.path()).unwrap();
        let mem = MemoryObjectStore::new();
        assert_eq!(
            disk.put(b"same bytes").unwrap(),
            mem.put(b"same bytes").unwrap()
        );
    }

    #[test]
    fn unique_names_do_not_collide() {
        let a: std::collections::HashSet<String> = (0..1000).map(|_| unique_name()).collect();
        assert_eq!(a.len(), 1000);
    }
}
