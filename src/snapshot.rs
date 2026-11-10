use crate::core::Engine;
use crate::core::schema;
use anyhow::{Result, ensure};
use rocksdb::{IteratorMode, ReadOptions};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::Path};
use tempfile::NamedTempFile;

const FORMAT: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    checksum: String,
    metadata: T,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Image {
    pub format: u32,
    pub seq: u64,
    pub system_id: String,
    /// Readable view derived from records; recovery verifies both agree.
    pub paths: Vec<Value>,
    pub records: Vec<Record>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub key: String,
    pub value: Value,
}

impl Record {
    pub(crate) fn decode<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(T::deserialize(&self.value)?)
    }

    pub(crate) fn bytes(&self) -> Result<Vec<u8>> {
        schema::encode(&self.value)
    }
}

pub(crate) fn decode(data: &Value) -> Result<Image> {
    let image = Image::deserialize(data)?;
    ensure!(image.format == FORMAT, "unsupported snapshot format");
    uuid::Uuid::parse_str(&image.system_id)?;
    Ok(image)
}

impl Engine {
    pub fn snapshot(&self) -> Result<Value> {
        let snapshot = self.inner.db.snapshot();
        let system_id = Self::decode::<String>(
            &snapshot
                .get(schema::SYSTEM_ID)?
                .ok_or_else(|| anyhow::anyhow!("missing system identity"))?,
        )?;
        let seq = snapshot
            .get(schema::REVISION)?
            .map(|bytes| Self::decode::<u64>(&bytes))
            .transpose()?
            .unwrap_or(0);
        let mut records = Vec::new();
        let mut options = ReadOptions::default();
        options.fill_cache(false);
        for item in snapshot.iterator_opt(IteratorMode::Start, options) {
            let (key, value) = item?;
            let key = String::from_utf8(key.to_vec())?;
            if schema::ephemeral(&key) {
                continue;
            }
            records.push(Record {
                key,
                value: schema::decode(&value)?,
            });
        }
        let image = Image {
            format: FORMAT,
            seq,
            system_id,
            paths: Vec::new(),
            records,
        };
        Ok(serde_json::to_value(crate::recovery::with_snapshot_paths(
            image,
        )?)?)
    }
}

fn checksum(metadata: &Value) -> Result<String> {
    let mut hash = Sha256::new();
    serde_json::to_writer(&mut hash, metadata)?;
    Ok(format!("{:x}", hash.finalize()))
}

pub fn write(engine: &Engine, path: &Path) -> Result<()> {
    let data = engine.snapshot()?;
    publish(&data, path)
}

pub(crate) fn publish(data: &Value, path: &Path) -> Result<()> {
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(
        &mut file,
        &Envelope {
            checksum: checksum(data)?,
            metadata: data,
        },
    )?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)?;
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

pub fn load(path: &Path) -> Result<Value> {
    let envelope: Envelope<Value> = serde_json::from_reader(fs::File::open(path)?)?;
    ensure!(
        envelope.checksum == checksum(&envelope.metadata)?,
        "snapshot checksum mismatch"
    );
    decode(&envelope.metadata)?;
    Ok(envelope.metadata)
}
