//! RocksDB configuration, typed access and atomic metadata writes.
use super::*;
use rocksdb::{DB, Options, Snapshot, WriteBatch, WriteOptions, backup::BackupEngine};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RocksDbCompression {
    None,
    #[default]
    Lz4,
}

/// Startup-only resource tuning. WAL and durability semantics are not optional.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RocksDbOptions {
    pub block_cache_mib: usize,
    pub write_buffer_mib: usize,
    pub max_write_buffers: i32,
    pub max_background_jobs: i32,
    pub compression: RocksDbCompression,
}

impl Default for RocksDbOptions {
    fn default() -> Self {
        Self {
            block_cache_mib: 128,
            write_buffer_mib: 32,
            max_write_buffers: 3,
            max_background_jobs: 2,
            compression: RocksDbCompression::Lz4,
        }
    }
}

impl RocksDbOptions {
    pub fn validate(&self) -> Result<()> {
        for (name, mib) in [
            ("block_cache_mib", self.block_cache_mib),
            ("write_buffer_mib", self.write_buffer_mib),
        ] {
            ensure!(
                mib > 0 && mib.checked_mul(1024 * 1024).is_some(),
                "rocksdb.{name} must be positive and fit in addressable memory"
            );
        }
        ensure!(
            self.max_write_buffers >= 2,
            "rocksdb.max_write_buffers must be at least 2"
        );
        ensure!(
            self.max_background_jobs > 0,
            "rocksdb.max_background_jobs must be positive"
        );
        Ok(())
    }

    fn build(&self) -> Result<Options> {
        self.validate()?;
        let mut options = Options::default();
        options.create_if_missing(true);
        options.set_compression_type(match self.compression {
            RocksDbCompression::None => rocksdb::DBCompressionType::None,
            RocksDbCompression::Lz4 => rocksdb::DBCompressionType::Lz4,
        });
        options.set_write_buffer_size(self.write_buffer_mib * 1024 * 1024);
        options.set_max_write_buffer_number(self.max_write_buffers);
        options.set_max_background_jobs(self.max_background_jobs);
        let mut blocks = rocksdb::BlockBasedOptions::default();
        blocks.set_block_cache(&rocksdb::Cache::new_lru_cache(
            self.block_cache_mib * 1024 * 1024,
        ));
        blocks.set_bloom_filter(10.0, false);
        options.set_block_based_table_factory(&blocks);
        Ok(options)
    }
}

/// Owns the database and the only storage-level read/modify/write operation.
/// Domain callers protect business decisions with namespace/file locks; a batch
/// makes their writes atomic but does not protect reads performed before it.
/// Queue locks never acquire domain locks and are held only for short DB work.
pub(crate) struct MetadataStore {
    db: DB,
    queues: [Mutex<()>; 64],
}

impl MetadataStore {
    fn open(path: &Path, options: &Options) -> Result<Self> {
        // Paths exported to another process must not depend on this process's cwd.
        fs::create_dir_all(path)?;
        let path = fs::canonicalize(path)?;
        Ok(Self {
            db: DB::open(options, &path)?,
            queues: std::array::from_fn(|_| Mutex::new(())),
        })
    }
    pub(crate) fn path(&self) -> &Path {
        self.db.path()
    }
    pub(crate) fn snapshot(&self) -> Snapshot<'_> {
        self.db.snapshot()
    }
    pub(crate) fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.db
            .get_pinned(key)?
            .map(|bytes| {
                schema::decode(&bytes).with_context(|| format!("invalid metadata value at {key}"))
            })
            .transpose()
    }
    pub(crate) fn scan<T: DeserializeOwned>(&self, prefix: &str) -> Result<Vec<(String, T)>> {
        let mut out = Vec::new();
        for item in self
            .db
            .iterator(IteratorMode::From(prefix.as_bytes(), Direction::Forward))
        {
            let (key, value) = item?;
            if !key.starts_with(prefix.as_bytes()) {
                break;
            }
            out.push((String::from_utf8(key.to_vec())?, schema::decode(&value)?));
        }
        Ok(out)
    }
    fn write(&self, puts: Vec<(String, Vec<u8>)>, deletes: Vec<String>, sync: bool) -> Result<()> {
        let mut batch = WriteBatch::default();
        for key in deletes {
            batch.delete(key);
        }
        for (key, value) in puts {
            batch.put(key, value);
        }
        let mut options = WriteOptions::default();
        options.set_sync(sync);
        self.db.write_opt(batch, &options)?;
        Ok(())
    }
    pub(crate) fn create_backup(&self, backup: &mut BackupEngine) -> Result<()> {
        backup.create_new_backup_flush(&self.db, true)?;
        Ok(())
    }
    /// Replace a catalog while it is offline (restore only).
    pub(crate) fn replace(&self, records: Vec<(String, Vec<u8>)>) -> Result<()> {
        let mut deletes = Vec::new();
        for item in self.db.iterator(IteratorMode::Start) {
            deletes.push(String::from_utf8(item?.0.to_vec())?);
        }
        self.write(records, deletes, true)
    }
    fn sync(&self) -> Result<()> {
        self.db.flush_wal(true)?;
        Ok(())
    }
    pub(crate) fn schedule(
        &self,
        kind: schema::WorkKind,
        id: &str,
        due: Option<u64>,
    ) -> Result<()> {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let pointer = schema::queue_at(kind, id);
        let mut hash = DefaultHasher::new();
        pointer.hash(&mut hash);
        let _guard = self.queues[hash.finish() as usize % self.queues.len()].lock();
        let old: Option<u64> = self.get(&pointer)?;
        if old == due {
            return Ok(());
        }
        let mut deletes = vec![pointer.clone()];
        if let Some(old) = old {
            deletes.push(schema::queue(kind, old, id));
        }
        let puts = match due {
            Some(due) => vec![
                (pointer, schema::encode(&due)?),
                (schema::queue(kind, due, id), schema::encode(&id)?),
            ],
            None => Vec::new(),
        };
        self.write(puts, deletes, false)
    }
}

impl Engine {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_options(path, &RocksDbOptions::default())
    }

    pub fn open_with_options(path: &Path, settings: &RocksDbOptions) -> Result<Self> {
        let options = settings.build()?;
        let db = MetadataStore::open(path, &options)?;
        // Never replace this inode: surviving clients may still hold OFD pins.
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(db.path().join("access.lock"))?;
        let engine = Self {
            inner: Arc::new(Inner {
                db,
                namespace: RwLock::new(()),
                files: Mutex::new(HashMap::new()),
                maintenance: Mutex::new(()),
                direct: Arc::new(crate::ipc::direct::DirectAccess::new()),
            }),
        };
        if let Some(format) = engine.kv_get::<String>(schema::SYSTEM_FORMAT)? {
            ensure!(
                format == schema::FORMAT,
                "unsupported metadata format: {format}"
            );
            let _: Record = engine
                .require(schema::ROOT)
                .context("missing namespace root")?;
            let _: String = engine
                .require(schema::SYSTEM_ID)
                .context("missing system identity")?;
        } else {
            ensure!(
                engine
                    .inner
                    .db
                    .snapshot()
                    .iterator(IteratorMode::Start)
                    .next()
                    .is_none(),
                "nonempty metadata database has no format marker"
            );
            engine.atomic(
                vec![
                    Self::encoded(schema::SYSTEM_FORMAT.into(), &schema::FORMAT)?,
                    Self::encoded(
                        schema::ROOT.into(),
                        &Record::new("root".into(), None, "".into(), true),
                    )?,
                    Self::encoded(schema::SYSTEM_ID.into(), &Uuid::new_v4().to_string())?,
                ],
                vec![],
                true,
            )?;
        }
        Ok(engine)
    }
    pub(crate) fn decode<T: DeserializeOwned>(v: &[u8]) -> Result<T> {
        schema::decode(v)
    }
    pub(crate) fn kv_get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.inner.db.get(key)
    }
    pub(crate) fn require<T: DeserializeOwned>(&self, key: &str) -> Result<T> {
        self.kv_get(key)?.ok_or_else(|| failure(libc::ENOENT))
    }
    pub(crate) fn scan<T: DeserializeOwned>(&self, prefix: &str) -> Result<Vec<(String, T)>> {
        self.inner.db.scan(prefix)
    }
    pub(crate) fn put_record<T: Serialize>(&self, k: &str, v: &T, sync: bool) -> Result<()> {
        self.atomic(vec![Self::encoded(k.into(), v)?], vec![], sync)
    }
    pub(crate) fn delete_record(&self, k: &str) -> Result<()> {
        self.atomic(vec![], vec![k.into()], false)
    }
    pub(crate) fn atomic(
        &self,
        puts: Vec<(String, Vec<u8>)>,
        deletes: Vec<String>,
        sync: bool,
    ) -> Result<()> {
        self.inner.db.write(puts, deletes, sync)
    }
    pub(crate) fn encoded<T: Serialize>(k: String, v: &T) -> Result<(String, Vec<u8>)> {
        Ok((k, schema::encode(v)?))
    }
    pub fn sync_metadata(&self) -> Result<()> {
        self.inner.db.sync()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_replace_keys_and_snapshots_never_split_namespace_updates() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let engine = Engine::open(&directory.path().join("metadata"))?;
        engine.atomic(
            vec![Engine::encoded("status/test".into(), &1u64)?],
            vec![],
            false,
        )?;
        engine.atomic(
            vec![Engine::encoded("status/test".into(), &2u64)?],
            vec!["status/test".into()],
            false,
        )?;
        assert_eq!(engine.require::<u64>("status/test")?, 2);
        std::thread::scope(|scope| -> Result<()> {
            let writer = scope.spawn(|| -> Result<()> {
                for index in 0..200 {
                    engine.mkdir(&format!("/directory-{index}"))?;
                }
                Ok(())
            });
            for _ in 0..100 {
                let view = engine.inner.db.snapshot();
                let mut entries = 0;
                let mut nodes = 0;
                for item in view.iterator(IteratorMode::Start) {
                    let (key, value) = item?;
                    if key.starts_with(schema::ENTRIES.as_bytes()) {
                        let id: String = schema::decode(&value)?;
                        let record: Record =
                            schema::decode(&view.get(schema::node(&id))?.unwrap())?;
                        assert_eq!(record.node.id, id);
                        entries += 1;
                    } else if key.starts_with(schema::NODES.as_bytes()) {
                        nodes += 1;
                    }
                }
                assert_eq!(entries + 1, nodes);
            }
            writer.join().unwrap()?;
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn tuning_can_change_when_reopening() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("metadata");
        let mut options = RocksDbOptions {
            block_cache_mib: 8,
            write_buffer_mib: 2,
            max_write_buffers: 2,
            max_background_jobs: 1,
            compression: RocksDbCompression::None,
        };
        let engine = Engine::open_with_options(&path, &options)?;
        engine.mkdir("/directory")?;
        let id = engine.stat("/directory")?.id;
        engine.sync_metadata()?;
        drop(engine);
        options.block_cache_mib = 16;
        options.compression = RocksDbCompression::Lz4;
        let engine = Engine::open_with_options(&path, &options)?;
        assert_eq!(engine.stat("/directory")?.id, id);
        Ok(())
    }

    #[test]
    fn incompatible_or_damaged_catalog_is_never_reinitialized() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("metadata");
        let engine = Engine::open(&path)?;
        engine.put_record(schema::SYSTEM_FORMAT, &"future-format", true)?;
        drop(engine);
        assert!(
            Engine::open(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("unsupported metadata format")
        );
        let db = DB::open(&Options::default(), &path)?;
        db.delete(schema::SYSTEM_FORMAT)?;
        drop(db);
        assert!(
            Engine::open(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("no format marker")
        );
        Ok(())
    }
}
