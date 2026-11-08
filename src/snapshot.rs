use crate::core::Engine;
use anyhow::{Result, ensure};
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
    ensure!(
        envelope.metadata["format"] == 1,
        "unsupported snapshot format"
    );
    Ok(envelope.metadata)
}
