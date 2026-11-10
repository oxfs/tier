use anyhow::Result;
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use tempfile::tempdir;
use tier::{
    backup::{self, Kind, Schedule},
    core::{Engine, Policy},
    recovery,
};

fn policy(write_cache: &str, persistent_storage: &[&str]) -> Result<Policy> {
    Ok(serde_json::from_value(
        json!({"write_cache":[write_cache],"persistent_storage":persistent_storage,"persistent_copies":persistent_storage.len(),"persist_delay_secs":0,"low_water":99,"high_water":100}),
    )?)
}

fn schedule(engine: &Engine, destinations: Vec<PathBuf>, seconds: u64, keep: usize) -> Result<()> {
    engine.set_backup_schedule(
        Kind::Metadata,
        &Schedule {
            enabled: true,
            directories: destinations,
            seconds,
            keep,
        },
    )
}

#[test]
fn restore_namespace_from_metadata_backup_with_a_single_surviving_disk() -> Result<()> {
    let work = tempdir()?;
    let root = work.path();
    let db = root.join("source.db");
    let engine = Engine::open(&db)?;
    for name in ["hot", "disk-a", "disk-b"] {
        fs::create_dir(root.join(name))?;
        engine.register_target(name, &root.join(name), None, true)?;
    }
    engine.set_policy("/", &policy("hot", &["disk-a", "disk-b"])?)?;
    engine.mkdir("/photos")?;
    engine.put("/photos/image", b"old")?;
    engine.put("/photos/image", b"new")?;
    engine.automatic_reconcile()?;
    engine.rename("/photos", "/albums")?;
    let repository = root.join("backups");
    fs::create_dir(&repository)?;
    schedule(&engine, vec![repository.clone()], 3600, 2)?;
    engine.backup_tick(Kind::Metadata)?;
    drop(engine);
    let moved = root.join("rehomed");
    fs::rename(root.join("disk-a"), &moved)?;
    fs::rename(root.join("hot"), root.join("offline-hot"))?;
    fs::rename(root.join("disk-b"), root.join("offline-b"))?;
    let out = root.join("extract");
    let report =
        recovery::inspect_backup(&repository, None, std::slice::from_ref(&moved), Some(&out))?;
    assert_eq!(report["verified"], json!(["albums/image"]));
    assert_eq!(fs::read(out.join("albums/image"))?, b"new");
    let recovered = root.join("recovered.db");
    recovery::restore_backup(
        &repository,
        None,
        &recovered,
        &BTreeMap::from([("disk-a".into(), moved)]),
        true,
        &Default::default(),
    )?;
    assert_eq!(Engine::open(&recovered)?.get("/albums/image")?, b"new");
    assert!(
        recovery::restore_backup(
            &repository,
            None,
            &recovered,
            &BTreeMap::new(),
            false,
            &Default::default()
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn metadata_backups_continue_when_one_destination_is_offline() -> Result<()> {
    let work = tempdir()?;
    let engine = Engine::open(&work.path().join("tier.db"))?;
    let a = work.path().join("a");
    let b = work.path().join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    schedule(&engine, vec![a.clone(), b.clone()], 30, 1)?;
    engine.backup_tick(Kind::Metadata)?;
    let latest = |dir: &Path| backup::list(dir);
    assert_eq!(latest(&a)?.len(), 1);
    fs::rename(&a, work.path().join("offline"))?;
    fs::create_dir(&a)?;
    engine.mkdir("/documents")?;
    assert!(engine.backup_tick(Kind::Metadata).is_err());
    assert!(fs::read_dir(&a)?.next().is_none());
    let files = latest(&b)?;
    assert_eq!(files.len(), 1);
    let restored = work.path().join("restored");
    recovery::restore_backup(
        &b,
        None,
        &restored,
        &BTreeMap::new(),
        false,
        &Default::default(),
    )?;
    assert!(Engine::open(&restored)?.stat("/documents")?.is_dir);
    Ok(())
}

#[test]
fn cloned_engine_exposes_concurrent_namespace_mutations() -> Result<()> {
    let work = tempdir()?;
    let engine = Engine::open(&work.path().join("metadata"))?;
    std::thread::scope(|scope| -> Result<()> {
        let handles = (0..8)
            .map(|i| {
                let engine = engine.clone();
                scope.spawn(move || engine.mkdir(&format!("/directory-{i}")))
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap()?;
        }
        Ok(())
    })?;
    assert_eq!(engine.list("/")?.len(), 8);
    assert!(Engine::open(&work.path().join("metadata")).is_err());
    Ok(())
}

#[test]
fn scheduled_backups_obey_interval_and_forced_backups_apply_retention() -> Result<()> {
    let work = tempdir()?;
    let engine = Engine::open(&work.path().join("metadata"))?;
    let destination = work.path().join("snapshots");
    fs::create_dir(&destination)?;
    schedule(&engine, vec![destination.clone()], 3600, 2)?;
    engine.schedule_backup(Kind::Metadata)?;
    for _ in 0..5 {
        engine.schedule_backup(Kind::Metadata)?;
    }
    let count = || -> Result<usize> { Ok(backup::list(&destination)?.len()) };
    assert_eq!(count()?, 1);
    for i in 0..8 {
        engine.mkdir(&format!("/directory-{i}"))?;
        engine.backup_tick(Kind::Metadata)?;
    }
    assert_eq!(count()?, 2);
    Ok(())
}

#[test]
fn offline_backup_destination_backs_off_without_delaying_healthy_targets() -> Result<()> {
    let root = tempdir()?;
    let engine = Engine::open(&root.path().join("metadata"))?;
    let a = root.path().join("a");
    let b = root.path().join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    schedule(&engine, vec![a.clone(), b.clone()], 60, 2)?;
    engine.schedule_backup(Kind::Metadata)?;
    fs::rename(&a, root.path().join("offline"))?;
    engine.mkdir("/new")?;
    assert!(engine.backup_tick(Kind::Metadata).is_err());
    // Failed destinations back off; repeating a scheduler tick does not repeat I/O.
    engine.schedule_backup(Kind::Metadata)?;
    let status = engine.state()?;
    assert!(
        status["backup_status"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["path"] == b.to_str().unwrap()
                && s["error"].is_null()
                && s["time"].as_u64().is_some_and(|time| time > 0))
    );
    assert!(engine.backup_tick(Kind::Metadata).is_err());
    Ok(())
}
