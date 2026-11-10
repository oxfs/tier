//! RocksDB configuration, typed access and atomic metadata writes.
use super::*;
use rocksdb::{Options, TransactionDBOptions, WriteOptions};
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

impl Engine {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_options(path, &RocksDbOptions::default())
    }

    pub fn open_with_options(path: &Path, settings: &RocksDbOptions) -> Result<Self> {
        let options = settings.build()?;
        let db = TransactionDB::open(&options, &TransactionDBOptions::default(), path)?;
        let engine = Self {
            inner: Arc::new(Inner {
                db,
                namespace: RwLock::new(()),
                files: Mutex::new(HashMap::new()),
                maintenance: Mutex::new(()),
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
        if engine.kv_get::<String>(schema::BACKUP_LINEAGE)?.is_none() {
            engine.put_record(schema::BACKUP_LINEAGE, &Uuid::new_v4().to_string(), true)?;
        }
        Ok(engine)
    }
    pub(crate) fn decode<T: DeserializeOwned>(v: &[u8]) -> Result<T> {
        schema::decode(v)
    }
    pub(crate) fn kv_get<T: DeserializeOwned>(&self, k: &str) -> Result<Option<T>> {
        self.inner
            .db
            .get(k)?
            .map(|v| schema::decode(&v).with_context(|| format!("invalid metadata value at {k}")))
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
            out.push((String::from_utf8(k.to_vec())?, schema::decode(&v)?));
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
        Ok(self.kv_get(schema::REVISION)?.unwrap_or(0))
    }
    pub(crate) fn atomic(
        &self,
        puts: Vec<(String, Vec<u8>)>,
        deletes: Vec<String>,
        sync: bool,
    ) -> Result<()> {
        let business = puts
            .iter()
            .map(|(k, _)| k)
            .chain(deletes.iter())
            .any(|k| !schema::ephemeral(k) && k != schema::REVISION);
        let mut opts = WriteOptions::default();
        opts.set_sync(sync);
        let tx = self
            .inner
            .db
            .transaction_opt(&opts, &rocksdb::TransactionOptions::default());
        let revision = if business {
            Some(
                tx.get_for_update(schema::REVISION, true)?
                    .map(|v| schema::decode::<u64>(&v))
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
                schema::REVISION,
                schema::encode(
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
        Ok((k, schema::encode(v)?))
    }
    pub fn sync_metadata(&self) -> Result<()> {
        let mut options = WriteOptions::default();
        options.set_sync(true);
        self.inner
            .db
            .put_opt(schema::BARRIER, schema::encode(&now()?)?, &options)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let db: TransactionDB =
            TransactionDB::open(&Options::default(), &TransactionDBOptions::default(), &path)?;
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
