//! Portable live-namespace file mappings; CSV is not a metadata restore format.
use crate::core::Engine;
use anyhow::Result;
use std::{fs, path::Path};
use tempfile::NamedTempFile;

pub fn write(engine: &Engine, path: &Path) -> Result<()> {
    let rows = crate::recovery::export_rows(engine)?;
    publish(&rows, path)
}

pub(crate) fn publish(rows: &[(String, String)], path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = NamedTempFile::new_in(parent)?;
    {
        let mut writer = csv::Writer::from_writer(&mut file);
        writer.write_record(["logical_path", "physical_path"])?;
        for (logical, physical) in rows {
            writer.write_record([logical, physical])?;
        }
        writer.flush()?;
    }
    file.as_file().sync_all()?;
    file.persist_noclobber(path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
