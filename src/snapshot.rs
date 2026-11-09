use crate::core::Engine;
use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rocksdb::IteratorMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, io::Write, path::Path};
use tempfile::NamedTempFile;

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
    pub records: Vec<Record>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Record {
    pub key: String,
    pub value: String,
}

impl Record {
    pub(crate) fn bytes(&self) -> Result<Vec<u8>> {
        Ok(STANDARD.decode(&self.value)?)
    }
}

pub(crate) fn decode(data: &Value) -> Result<Image> {
    let image: Image = serde_json::from_value(data.clone())?;
    ensure!(image.format == 2, "unsupported snapshot format");
    uuid::Uuid::parse_str(&image.system_id)?;
    Ok(image)
}

impl Engine {
    pub fn snapshot(&self) -> Result<Value> {
        let snapshot = self.inner.db.snapshot();
        let system_id = Self::decode::<String>(
            &snapshot
                .get("system/id")?
                .ok_or_else(|| anyhow::anyhow!("missing system identity"))?,
        )?;
        let seq = snapshot
            .get("system/revision")?
            .map(|bytes| Self::decode::<u64>(&bytes))
            .transpose()?
            .unwrap_or(0);
        let mut records = Vec::new();
        for item in snapshot.iterator(IteratorMode::Start) {
            let (key, value) = item?;
            let key = String::from_utf8(key.to_vec())?;
            if key.starts_with("backup/status/")
                || key.starts_with("status/")
                || key.starts_with("queue/")
                || key.starts_with("queue_at/")
                || key == "backup/lineage"
            {
                continue;
            }
            records.push(Record {
                key,
                value: STANDARD.encode(value),
            });
        }
        Ok(serde_json::to_value(Image {
            format: 2,
            seq,
            system_id,
            records,
        })?)
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
