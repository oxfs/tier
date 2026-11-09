mod catalog;
mod file;
mod reconcile;
mod report;
mod target;
use anyhow::{Context, Result, bail, ensure};
pub use file::FileHandle;
use parking_lot::{Mutex, RwLock};
use rocksdb::{
    Direction, IteratorMode, Options, TransactionDB, TransactionDBOptions, WriteOptions,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub write_cache: Vec<String>,
    #[serde(default)]
    pub persistent_storage: Vec<String>,
    #[serde(default)]
    pub persistent_copies: usize,
    #[serde(default)]
    pub persist_delay_secs: u64,
    #[serde(default)]
    pub direct_fallback: bool,
    #[serde(default)]
    pub manual_repair: bool,
    #[serde(default)]
    pub repair_grace_secs: u64,
    #[serde(default = "default_low")]
    pub low_water: u8,
    #[serde(default = "default_high")]
    pub high_water: u8,
    #[serde(default = "default_ttl")]
    pub trash_ttl_secs: u64,
}
fn default_low() -> u8 {
    70
}
fn default_high() -> u8 {
    90
}
fn default_ttl() -> u64 {
    604800
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            write_cache: vec![],
            persistent_storage: vec![],
            persistent_copies: 0,
            persist_delay_secs: 0,
            direct_fallback: false,
            manual_repair: false,
            repair_grace_secs: 0,
            low_water: 70,
            high_water: 90,
            trash_ttl_secs: 604800,
        }
    }
}
impl Policy {
    pub(crate) fn desired(&self) -> (&[String], usize) {
        if self.persistent_copies > 0 {
            (&self.persistent_storage, self.persistent_copies)
        } else {
            (&self.write_cache, 1)
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: SystemTime,
    pub atime: SystemTime,
    pub ctime: SystemTime,
    pub btime: SystemTime,
    pub version: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub node: Node,
    pub parent: Option<String>,
    pub name: String,
    pub primary: Option<String>,
    pub hash: Option<String>,
    pub dirty: bool,
    pub placement: Option<(u64, Vec<String>, usize)>,
    pub closed_at: u64,
    pub incident: Option<u64>,
    pub error: Option<String>,
}
impl Record {
    fn new(id: String, parent: Option<String>, name: String, is_dir: bool) -> Self {
        let time = SystemTime::now();
        Self {
            node: Node {
                id,
                is_dir,
                size: 0,
                mtime: time,
                atime: time,
                ctime: time,
                btime: time,
                version: 0,
            },
            parent,
            name,
            primary: None,
            hash: None,
            dirty: false,
            placement: None,
            closed_at: 0,
            incident: None,
            error: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Copy {
    pub node: String,
    pub target: String,
    pub object: String,
    pub version: u64,
    pub hash: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Target {
    pub name: String,
    pub path: PathBuf,
    pub identity: String,
    pub domain: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Trash {
    pub id: i64,
    pub node: String,
    pub original: String,
    pub deleted: u64,
    pub expires: u64,
    pub policy: Policy,
    pub archive_policy: Option<crate::archive::ArchivePolicy>,
}
#[derive(Default)]
pub(crate) struct FileState {
    pub writer: bool,
    pub handles: usize,
    pub body: Option<file::Body>,
}
pub(crate) struct Inner {
    pub db: TransactionDB,
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
pub(crate) fn key(prefix: &str, id: &str) -> String {
    format!("{prefix}/{id}")
}
fn entry(parent: &str, name: &str) -> String {
    format!(
        "entry/{parent}/{}",
        unicase::UniCase::new(name).to_folded_case()
    )
}
pub(crate) fn failure(errno: i32) -> anyhow::Error {
    io::Error::from_raw_os_error(errno).into()
}
impl Engine {
    pub fn open(path: &Path) -> Result<Self> {
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_compression_type(rocksdb::DBCompressionType::Lz4);
        options.set_write_buffer_size(32 * 1024 * 1024);
        options.set_max_write_buffer_number(3);
        options.set_max_background_jobs(2);
        let mut blocks = rocksdb::BlockBasedOptions::default();
        blocks.set_block_cache(&rocksdb::Cache::new_lru_cache(128 * 1024 * 1024));
        blocks.set_bloom_filter(10.0, false);
        options.set_block_based_table_factory(&blocks);
        let db = TransactionDB::open(&options, &TransactionDBOptions::default(), path)?;
        let engine = Self {
            inner: Arc::new(Inner {
                db,
                namespace: RwLock::new(()),
                files: Mutex::new(HashMap::new()),
                maintenance: Mutex::new(()),
            }),
        };
        if let Some(format) = engine.kv_get::<String>("system/format")? {
            ensure!(format == "tier-rocksdb-v1", "unsupported metadata format");
        } else {
            engine.put_record("system/format", &"tier-rocksdb-v1".to_owned(), true)?;
        }
        if engine.kv_get::<Record>("node/root")?.is_none() {
            engine.atomic(
                vec![
                    Self::encoded(
                        "node/root".into(),
                        &Record::new("root".into(), None, "".into(), true),
                    )?,
                    Self::encoded("system/id".into(), &Uuid::new_v4().to_string())?,
                ],
                vec![],
                true,
            )?;
        }
        if engine.kv_get::<String>("backup/lineage")?.is_none() {
            engine.put_record("backup/lineage", &Uuid::new_v4().to_string(), true)?;
        }
        Ok(engine)
    }
    pub(crate) fn decode<T: DeserializeOwned>(v: &[u8]) -> Result<T> {
        Ok(postcard::from_bytes(v)?)
    }
    pub(crate) fn kv_get<T: DeserializeOwned>(&self, k: &str) -> Result<Option<T>> {
        self.inner
            .db
            .get(k)?
            .map(|v| postcard::from_bytes(&v).map_err(Into::into))
            .transpose()
    }
    pub(crate) fn require<T: DeserializeOwned>(&self, k: &str) -> Result<T> {
        self.kv_get(k)?.ok_or_else(|| failure(libc::ENOENT))
    }
    pub(crate) fn scan<T: DeserializeOwned>(&self, prefix: &str) -> Result<Vec<(String, T)>> {
        let mut out = Vec::new();
        for item in self
            .inner
            .db
            .iterator(IteratorMode::From(prefix.as_bytes(), Direction::Forward))
        {
            let (k, v) = item?;
            if !k.starts_with(prefix.as_bytes()) {
                break;
            }
            out.push((String::from_utf8(k.to_vec())?, postcard::from_bytes(&v)?));
        }
        Ok(out)
    }
    pub(crate) fn put_record<T: Serialize>(&self, k: &str, v: &T, sync: bool) -> Result<()> {
        self.atomic(vec![Self::encoded(k.into(), v)?], vec![], sync)
    }
    pub(crate) fn delete_record(&self, k: &str) -> Result<()> {
        self.atomic(vec![], vec![k.into()], false)
    }
    pub(crate) fn business_revision(&self) -> Result<u64> {
        Ok(self.kv_get("system/revision")?.unwrap_or(0))
    }
    pub(crate) fn atomic(
        &self,
        puts: Vec<(String, Vec<u8>)>,
        deletes: Vec<String>,
        sync: bool,
    ) -> Result<()> {
        let business = puts.iter().map(|(k, _)| k).chain(deletes.iter()).any(|k| {
            !k.starts_with("queue/")
                && !k.starts_with("queue_at/")
                && !k.starts_with("backup/status/")
                && k != "backup/lineage"
                && !k.starts_with("status/")
                && k != "system/revision"
        });
        let mut opts = WriteOptions::default();
        opts.set_sync(sync);
        let tx = self
            .inner
            .db
            .transaction_opt(&opts, &rocksdb::TransactionOptions::default());
        let revision = if business {
            Some(
                tx.get_for_update("system/revision", true)?
                    .map(|v| postcard::from_bytes::<u64>(&v))
                    .transpose()?
                    .unwrap_or(0),
            )
        } else {
            None
        };
        let mut changed = false;
        for (k, v) in puts {
            if tx.get(&k)?.as_deref() != Some(v.as_slice()) {
                tx.put(k, v)?;
                changed = true;
            }
        }
        for k in deletes {
            if tx.get(&k)?.is_some() {
                tx.delete(k)?;
                changed = true;
            }
        }
        if changed && let Some(revision) = revision {
            tx.put(
                "system/revision",
                postcard::to_allocvec(
                    &revision
                        .checked_add(1)
                        .context("metadata revision exhausted")?,
                )?,
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn encoded<T: Serialize>(k: String, v: &T) -> Result<(String, Vec<u8>)> {
        Ok((k, postcard::to_allocvec(v)?))
    }
    pub(crate) fn record(&self, id: &str) -> Result<Record> {
        self.require(&key("node", id))
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
            .scan::<Copy>(&format!("copy/{id}/"))?
            .into_iter()
            .map(|(_, v)| v)
            .collect())
    }
    pub(crate) fn view_get<T: DeserializeOwned>(
        view: &rocksdb::SnapshotWithThreadMode<'_, TransactionDB>,
        key: &str,
    ) -> Result<Option<T>> {
        view.get(key)?.map(|bytes| Self::decode(&bytes)).transpose()
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
                Self::view_get(&view, &key("node", &id))?.ok_or_else(|| failure(libc::ENOENT))?;
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
        let mut id = id.to_owned();
        let mut seen = HashSet::new();
        loop {
            ensure!(seen.insert(id.clone()), "namespace cycle");
            if let Some(policy) = Self::view_get(&view, &key("policy", &id))? {
                return Ok(policy);
            }
            let node: Record =
                Self::view_get(&view, &key("node", &id))?.ok_or_else(|| failure(libc::ENOENT))?;
            if let Some(parent) = node.parent {
                id = parent;
            } else {
                for item in view.iterator(IteratorMode::From(b"trash/", Direction::Forward)) {
                    let (key, value) = item?;
                    if !key.starts_with(b"trash/") {
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
    pub fn sync_metadata(&self) -> Result<()> {
        let mut options = WriteOptions::default();
        options.set_sync(true);
        self.inner
            .db
            .put_opt("status/barrier", postcard::to_allocvec(&now()?)?, &options)?;
        Ok(())
    }
}
impl Engine {
    pub(crate) fn schedule_work(&self, kind: &str, id: &str, due: u64) -> Result<()> {
        let pointer = format!("queue_at/{kind}/{id}");
        let tx = self.inner.db.transaction();
        let old = tx
            .get_for_update(&pointer, true)?
            .map(|v| Self::decode::<u64>(&v))
            .transpose()?;
        if old == Some(due) {
            return Ok(());
        }
        if let Some(old) = old {
            tx.delete(format!("queue/{kind}/{old:020}/{id}"))?;
        }
        tx.put(&pointer, postcard::to_allocvec(&due)?)?;
        tx.put(
            format!("queue/{kind}/{due:020}/{id}"),
            postcard::to_allocvec(&id)?,
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn schedule_file(&self, id: &str) -> Result<()> {
        let r = self.record(id)?;
        let p = self.policy(id)?;
        self.schedule_work(
            "local",
            id,
            r.closed_at.saturating_add(p.persist_delay_secs),
        )?;
        let delay = self.archive_policy(id)?.map(|p| p.delay_secs).unwrap_or(0);
        self.schedule_work(
            "archive",
            id,
            r.closed_at.saturating_add(delay.max(p.persist_delay_secs)),
        )
    }
    pub(crate) fn due_work(&self, kind: &str, manual: bool) -> Result<Vec<String>> {
        if manual {
            return Ok(self
                .scan::<Record>("node/")?
                .into_iter()
                .filter(|(_, r)| !r.node.is_dir)
                .map(|(_, r)| r.node.id)
                .collect());
        }
        let prefix = format!("queue/{kind}/");
        let limit = format!("queue/{kind}/{:020}/~", now()?);
        let mut ids = vec![];
        for item in self
            .inner
            .db
            .iterator(IteratorMode::From(prefix.as_bytes(), Direction::Forward))
        {
            let (k, v) = item?;
            if !k.starts_with(prefix.as_bytes()) || k.as_ref() > limit.as_bytes() {
                break;
            }
            ids.push(Self::decode(&v)?);
            if ids.len() >= 128 {
                break;
            }
        }
        Ok(ids)
    }
    pub(crate) fn reschedule_work(&self, kind: &str, id: &str, failed: bool) -> Result<()> {
        if let Ok(r) = self.record(id)
            && self.live(id)?
        {
            let lock = self.file_lock(id);
            let state = lock.lock();
            let delay = if state.writer {
                5
            } else if failed {
                60
            } else {
                3600
            };
            let due = if r.dirty && !state.writer {
                let p = self.policy(id)?;
                r.closed_at
                    .saturating_add(p.persist_delay_secs)
                    .max(now()?.saturating_add(5))
            } else {
                now()?.saturating_add(delay)
            };
            self.schedule_work(kind, id, due)?;
        } else {
            let pointer = format!("queue_at/{kind}/{id}");
            if let Some(old) = self.kv_get::<u64>(&pointer)? {
                self.atomic(
                    vec![],
                    vec![pointer, format!("queue/{kind}/{old:020}/{id}")],
                    false,
                )?;
            }
        }
        Ok(())
    }
}
impl Engine {
    pub(crate) fn rebuild_work_queue(&self) -> Result<()> {
        for (_, r) in self.scan::<Record>("node/")? {
            if !r.node.is_dir && self.live(&r.node.id)? {
                self.schedule_file(&r.node.id)?;
            }
        }
        Ok(())
    }
}
