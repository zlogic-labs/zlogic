use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::{HashAlgo, ObjectError, ObjectId, ObjectStore, Result, hex_of};

pub trait BlobTransport: Send + Sync {
    fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<()>;
    fn open(&self, id: &ObjectId) -> Result<Box<dyn Read + Send>>;
    fn exists(&self, id: &ObjectId) -> Result<bool>;
    fn size(&self, id: &ObjectId) -> Result<u64>;
}

pub struct RemoteObjectStore {
    transport: Arc<dyn BlobTransport>,
    cache: Option<Arc<dyn ObjectStore>>,
}

impl RemoteObjectStore {
    pub fn new(transport: Arc<dyn BlobTransport>) -> Self {
        Self {
            transport,
            cache: None,
        }
    }

    pub fn with_cache(mut self, cache: Arc<dyn ObjectStore>) -> Self {
        debug_assert_eq!(
            cache.algo(),
            HashAlgo::Sha256,
            "cache must use the same hashing scheme as the remote store, otherwise ids will not match"
        );
        self.cache = Some(cache);
        self
    }
}

impl ObjectStore for RemoteObjectStore {
    fn algo(&self) -> HashAlgo {
        HashAlgo::Sha256
    }

    fn put(&self, bytes: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::new(HashAlgo::Sha256, hex_of(&Sha256::digest(bytes)));
        if !self.transport.exists(&id)? {
            self.transport.put(&id, bytes)?;
        }
        if let Some(c) = &self.cache {
            let _ = c.put(bytes);
        }
        Ok(id)
    }

    fn put_path(&self, path: &Path) -> Result<ObjectId> {
        self.put(&std::fs::read(path)?)
    }

    fn open(&self, id: &ObjectId) -> Result<Box<dyn Read + Send>> {
        self.check_algo(id)?;
        if let Some(c) = &self.cache
            && c.exists(id).unwrap_or(false)
        {
            return c.open(id);
        }
        let mut r = self.transport.open(id)?;
        match &self.cache {
            Some(c) => {
                let mut buf = Vec::new();
                r.read_to_end(&mut buf)?;
                let got = ObjectId::new(HashAlgo::Sha256, hex_of(&Sha256::digest(&buf)));
                if &got != id {
                    return Err(ObjectError::Corrupt {
                        id: id.clone(),
                        reason: format!("remote returned content hashing to {got}"),
                    });
                }
                let _ = c.put(&buf);
                Ok(Box::new(Cursor::new(buf)))
            }
            None => Ok(r),
        }
    }

    fn exists(&self, id: &ObjectId) -> Result<bool> {
        self.check_algo(id)?;
        if let Some(c) = &self.cache
            && c.exists(id).unwrap_or(false)
        {
            return Ok(true);
        }
        self.transport.exists(id)
    }

    fn size(&self, id: &ObjectId) -> Result<u64> {
        self.check_algo(id)?;
        if let Some(c) = &self.cache
            && c.exists(id).unwrap_or(false)
        {
            return c.size(id);
        }
        self.transport.size(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryObjectStore;
    use std::collections::HashMap;
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeRemote {
        blobs: RwLock<HashMap<String, Vec<u8>>>,
        reads: AtomicUsize,
        corrupt: bool,
    }

    impl BlobTransport for FakeRemote {
        fn put(&self, id: &ObjectId, bytes: &[u8]) -> Result<()> {
            self.blobs
                .write()
                .unwrap()
                .insert(id.hex.clone(), bytes.to_vec());
            Ok(())
        }
        fn open(&self, id: &ObjectId) -> Result<Box<dyn Read + Send>> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            if self.corrupt {
                return Ok(Box::new(Cursor::new(b"tampered".to_vec())));
            }
            let m = self.blobs.read().unwrap();
            let b = m
                .get(&id.hex)
                .ok_or_else(|| ObjectError::NotFound(id.clone()))?
                .clone();
            Ok(Box::new(Cursor::new(b)))
        }
        fn exists(&self, id: &ObjectId) -> Result<bool> {
            Ok(self.blobs.read().unwrap().contains_key(&id.hex))
        }
        fn size(&self, id: &ObjectId) -> Result<u64> {
            let m = self.blobs.read().unwrap();
            m.get(&id.hex)
                .map(|b| b.len() as u64)
                .ok_or_else(|| ObjectError::NotFound(id.clone()))
        }
    }

    #[test]
    fn round_trips_through_the_transport() {
        let s = RemoteObjectStore::new(Arc::new(FakeRemote::default()));
        let id = s.put(b"payload").unwrap();
        assert_eq!(s.get(&id).unwrap(), b"payload");
        assert_eq!(s.size(&id).unwrap(), 7);
    }

    #[test]
    fn ids_match_local_stores() {
        let s = RemoteObjectStore::new(Arc::new(FakeRemote::default()));
        let mem = MemoryObjectStore::new();
        assert_eq!(s.put(b"x").unwrap(), mem.put(b"x").unwrap());
    }

    #[test]
    fn cache_hit_skips_the_network() {
        let remote = Arc::new(FakeRemote::default());
        let s =
            RemoteObjectStore::new(remote.clone()).with_cache(Arc::new(MemoryObjectStore::new()));

        let id = s.put(b"cached").unwrap();
        assert_eq!(s.get(&id).unwrap(), b"cached");
        assert_eq!(s.get(&id).unwrap(), b"cached");
        assert_eq!(
            remote.reads.load(Ordering::Relaxed),
            0,
            "put already wrote it to the cache, so the remote must not be read again"
        );
    }

    #[test]
    fn tampered_remote_content_is_rejected_not_cached() {
        let remote = Arc::new(FakeRemote {
            corrupt: true,
            ..Default::default()
        });
        let cache = Arc::new(MemoryObjectStore::new());
        let s = RemoteObjectStore::new(remote).with_cache(cache.clone());

        let id = ObjectId::new(
            HashAlgo::Sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert!(matches!(s.open(&id), Err(ObjectError::Corrupt { .. })));
        assert!(
            cache.is_empty(),
            "content whose id does not match must never enter the cache"
        );
    }
}
