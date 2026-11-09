use anyhow::Result;
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path};
use tempfile::tempdir;
use tier::{
    core::{Engine, Policy},
    recovery,
};

fn policy(write_cache: &str, persistent_storage: &[&str]) -> Result<Policy> {
    Ok(serde_json::from_value(
        json!({"write_cache":[write_cache],"persistent_storage":persistent_storage,"persistent_copies":persistent_storage.len(),"persist_delay_secs":0}),
    )?)
}

#[test]
fn restore_namespace_from_snapshot_with_a_single_surviving_disk() -> Result<()> {
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
    let snapshot = root.join("snapshot.json");
    tier::snapshot::write(&engine, &snapshot)?;
    drop(engine);
    let moved = root.join("rehomed");
    fs::rename(root.join("disk-a"), &moved)?;
    fs::rename(root.join("hot"), root.join("offline-hot"))?;
    fs::rename(root.join("disk-b"), root.join("offline-b"))?;
    let data = tier::snapshot::load(&snapshot)?;
    let out = root.join("extract");
    let report = recovery::inspect(&data, std::slice::from_ref(&moved), Some(&out))?;
    assert_eq!(report["verified"], json!(["albums/image"]));
    assert_eq!(fs::read(out.join("albums/image"))?, b"new");
    let recovered = root.join("recovered.db");
    recovery::import(
        &data,
        &recovered,
        &BTreeMap::from([("disk-a".into(), moved)]),
        true,
    )?;
    assert_eq!(Engine::open(&recovered)?.get("/albums/image")?, b"new");
    assert!(recovery::import(&data, &recovered, &BTreeMap::new(), false).is_err());
    Ok(())
}

#[test]
fn snapshots_continue_when_one_destination_is_offline() -> Result<()> {
    use tier::backup::Schedule;
    let work = tempdir()?;
    let engine = Engine::open(&work.path().join("tier.db"))?;
    let a = work.path().join("a");
    let b = work.path().join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    engine.schedule(&Schedule {
        directories: vec![a.clone(), b.clone()],
        seconds: 30,
        changes: 1,
        keep: 1,
        retain_days: 0,
        hourly: 0,
        daily: 0,
    })?;
    engine.backup_tick()?;
    let latest = |dir: &Path| -> Result<Vec<_>> {
        Ok(fs::read_dir(dir)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .collect())
    };
    assert_eq!(latest(&a)?.len(), 1);
    fs::rename(&a, work.path().join("offline"))?;
    fs::create_dir(&a)?;
    engine.mkdir("/documents")?;
    assert!(engine.backup_tick().is_err());
    assert!(latest(&a)?.is_empty());
    let files = latest(&b)?;
    assert_eq!(files.len(), 1);
    let id = engine.stat("/documents")?.id;
    assert!(
        tier::snapshot::load(&files[0].path())?["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["key"] == format!("node/{id}"))
    );
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
fn backup_bookkeeping_does_not_trigger_new_snapshots_and_retention_is_applied() -> Result<()> {
    let work = tempdir()?;
    let engine = Engine::open(&work.path().join("metadata"))?;
    let destination = work.path().join("snapshots");
    fs::create_dir(&destination)?;
    engine.schedule(&tier::backup::Schedule {
        directories: vec![destination.clone()],
        seconds: 3600,
        changes: 1,
        keep: 2,
        retain_days: 0,
        hourly: 0,
        daily: 0,
    })?;
    engine.schedule_backups()?;
    let first = engine.snapshot()?["seq"].clone();
    for _ in 0..5 {
        engine.schedule_backups()?;
    }
    assert_eq!(engine.snapshot()?["seq"], first);
    let count = || -> Result<usize> {
        Ok(fs::read_dir(&destination)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|file| file.path().extension().is_some_and(|e| e == "json"))
            .count())
    };
    assert_eq!(count()?, 1);
    for i in 0..8 {
        engine.mkdir(&format!("/directory-{i}"))?;
        engine.schedule_backups()?;
    }
    assert_eq!(count()?, 2);
    Ok(())
}

#[test]
fn offline_snapshot_destination_backs_off_without_delaying_healthy_targets() -> Result<()> {
    let root = tempdir()?;
    let engine = Engine::open(&root.path().join("metadata"))?;
    let a = root.path().join("a");
    let b = root.path().join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    engine.schedule(&tier::backup::Schedule {
        directories: vec![a.clone(), b.clone()],
        seconds: 60,
        changes: 1,
        keep: 2,
        retain_days: 0,
        hourly: 0,
        daily: 0,
    })?;
    engine.schedule_backups()?;
    fs::rename(&a, root.path().join("offline"))?;
    engine.mkdir("/new")?;
    assert!(engine.schedule_backups().is_err());
    // Failed destinations back off; repeating a scheduler tick does not repeat I/O.
    engine.schedule_backups()?;
    let status = engine.state()?;
    assert!(
        status["backup_status"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["path"] == b.to_str().unwrap() && s["lag"] == 0)
    );
    assert!(engine.backup_tick().is_err());
    Ok(())
}
