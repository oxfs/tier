//! Native metadata backup recovery and portable file-list exports.
use anyhow::Result;
use std::{
    fs,
    path::{Path, PathBuf},
};
use tier::{
    backup::{Kind, Schedule},
    core::{Engine, Policy},
};

#[path = "recovery/export.rs"]
mod export;
#[path = "recovery/integrity.rs"]
mod integrity;

fn setup(root: &Path) -> Result<(Engine, PathBuf)> {
    let disk = root.join("disk");
    fs::create_dir(&disk)?;
    let engine = Engine::open(&root.join("metadata"))?;
    engine.add_target("disk", &disk)?;
    engine.set_policy(
        "/",
        &Policy {
            write_cache: vec!["disk".into()],
            low_water: 99,
            high_water: 100,
            ..Default::default()
        },
    )?;
    Ok((engine, disk))
}
fn backup(engine: &Engine, root: &Path) -> Result<PathBuf> {
    let repository = root.join("backups");
    fs::create_dir(&repository)?;
    engine.set_backup_schedule(
        Kind::Metadata,
        &Schedule {
            enabled: true,
            directories: vec![repository.clone()],
            seconds: 3600,
            keep: 3,
        },
    )?;
    engine.backup_tick(Kind::Metadata)?;
    Ok(repository)
}
fn object(disk: &Path, id: &str) -> Result<PathBuf> {
    Ok(fs::read_dir(disk.join("objects").join(id))?
        .next()
        .unwrap()?
        .path())
}
