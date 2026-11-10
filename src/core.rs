mod catalog;
mod file;
mod model;
pub(crate) mod objects;
mod queue;
mod reconcile;
mod report;
pub(crate) mod schema;
mod store;
mod target;

pub use file::FileHandle;
pub(crate) use model::{Copy, Record, Target, Trash};
pub use model::{Node, Policy};
pub use store::{RocksDbCompression, RocksDbOptions};

use anyhow::{Context, Result, bail, ensure};
use parking_lot::{Mutex, RwLock};
use rocksdb::{Direction, IteratorMode};
use schema::entry;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Read, Seek, Write},
    path::Path,
    sync::{Arc, Weak},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Default)]
pub(crate) struct FileState {
    pub writer: bool,
    pub handles: usize,
    pub body: Option<file::Body>,
}
pub(crate) struct Inner {
    pub db: store::MetadataStore,
    // Namespace operations are short; no body I/O or remote calls under this lock.
    pub namespace: RwLock<()>,
    pub files: Mutex<HashMap<String, Weak<Mutex<FileState>>>>,
    pub maintenance: Mutex<()>,
}
#[derive(Clone)]
pub struct Engine {
    pub(crate) inner: Arc<Inner>,
}
pub(crate) fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
pub(crate) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.contains(['/', '\\', '\0'])
        && name != "."
        && name != ".."
}
fn split(path: &str) -> Result<(&str, &str)> {
    ensure!(path.starts_with('/') && path != "/", "invalid path");
    ensure!(path[1..].split('/').all(safe_name), "invalid path");
    let (p, n) = path.rsplit_once('/').unwrap();
    Ok((if p.is_empty() { "/" } else { p }, n))
}
pub(crate) fn fingerprint<R: Read + Seek>(r: &mut R) -> Result<(u64, String)> {
    r.rewind()?;
    let mut h = Sha256::new();
    let n = io::copy(r, &mut h)?;
    Ok((n, format!("{:x}", h.finalize())))
}
fn rank(names: &mut [String], id: &str) {
    names.sort_by_cached_key(|n| Sha256::digest(format!("{id}/{n}")).to_vec());
}
pub(crate) fn failure(errno: i32) -> anyhow::Error {
    io::Error::from_raw_os_error(errno).into()
}
impl Engine {
    pub(crate) fn record(&self, id: &str) -> Result<Record> {
        self.require(&schema::node(id))
    }
    pub(crate) fn file_lock(&self, id: &str) -> Arc<Mutex<FileState>> {
        let mut locks = self.inner.files.lock();
        if let Some(lock) = locks.get(id).and_then(Weak::upgrade) {
            return lock;
        }
        if locks.len() > 65536 {
            locks.retain(|_, w| w.strong_count() > 0);
        }
        let lock = Arc::new(Mutex::new(FileState::default()));
        locks.insert(id.into(), Arc::downgrade(&lock));
        lock
    }
    pub(crate) fn copies(&self, id: &str) -> Result<Vec<Copy>> {
        Ok(self
            .scan::<Copy>(&schema::copies(id))?
            .into_iter()
            .map(|(_, v)| v)
            .collect())
    }
    pub(crate) fn view_get<T: DeserializeOwned>(
        view: &rocksdb::Snapshot<'_>,
        key: &str,
    ) -> Result<Option<T>> {
        view.get_pinned(key)?
            .map(|bytes| Self::decode(&bytes))
            .transpose()
    }
    pub(crate) fn live(&self, id: &str) -> Result<bool> {
        Ok(self.path_for(id)?.is_some())
    }
    pub(crate) fn path_for(&self, id: &str) -> Result<Option<String>> {
        let view = self.inner.db.snapshot();
        let mut id = id.to_owned();
        let mut parts = Vec::new();
        let mut seen = HashSet::new();
        while id != "root" {
            ensure!(seen.insert(id.clone()), "namespace cycle");
            let node: Record =
                Self::view_get(&view, &schema::node(&id))?.ok_or_else(|| failure(libc::ENOENT))?;
            parts.push(node.name);
            let Some(parent) = node.parent else {
                return Ok(None);
            };
            id = parent;
        }
        parts.reverse();
        Ok(Some(format!("/{}", parts.join("/"))))
    }
    pub(crate) fn policy(&self, id: &str) -> Result<Policy> {
        let view = self.inner.db.snapshot();
        Self::policy_in_view(&view, id, None)
    }
    /// Caller holds the file lock, keeping this record stable through the write.
    /// Policy ancestry and the record come from the same snapshot; no cross-request cache.
    pub(crate) fn record_with_policy(&self, id: &str) -> Result<(Record, Policy)> {
        let view = self.inner.db.snapshot();
        let record: Record =
            Self::view_get(&view, &schema::node(id))?.ok_or_else(|| failure(libc::ENOENT))?;
        let policy = Self::policy_in_view(&view, id, Some(&record))?;
        Ok((record, policy))
    }
    fn policy_in_view(
        view: &rocksdb::Snapshot<'_>,
        id: &str,
        mut first: Option<&Record>,
    ) -> Result<Policy> {
        let mut id = id.to_owned();
        let mut seen = HashSet::new();
        loop {
            ensure!(seen.insert(id.clone()), "namespace cycle");
            if let Some(policy) = Self::view_get(view, &schema::policy(&id))? {
                return Ok(policy);
            }
            let parent = if let Some(record) = first.take() {
                record.parent.clone()
            } else {
                Self::view_get::<Record>(view, &schema::node(&id))?
                    .ok_or_else(|| failure(libc::ENOENT))?
                    .parent
            };
            if let Some(parent) = parent {
                id = parent;
            } else {
                for item in view.iterator(IteratorMode::From(
                    schema::TRASH.as_bytes(),
                    Direction::Forward,
                )) {
                    let (key, value) = item?;
                    if !key.starts_with(schema::TRASH.as_bytes()) {
                        break;
                    }
                    let trash: Trash = Self::decode(&value)?;
                    if trash.node == id {
                        return Ok(trash.policy);
                    }
                }
                bail!("no applicable storage policy");
            }
        }
    }
}
