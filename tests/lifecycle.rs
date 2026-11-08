use anyhow::Result;
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path};
use tempfile::tempdir;
use tier::{
    core::{Engine, Policy},
    recovery,
};

fn policy(front: &str, long: &[&str]) -> Result<Policy> {
    Ok(serde_json::from_value(
        json!({"front":[front],"front_count":1,"long":long,"long_count":long.len(),"delay_secs":0}),
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
    use tier::backup::{self, Schedule};
    let work = tempdir()?;
    let engine = Engine::open(&work.path().join("tier.db"))?;
    backup::init(&engine)?;
    let a = work.path().join("a");
    let b = work.path().join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    engine.schedule(&Schedule {
        directories: vec![a.clone(), b.clone()],
        seconds: 30,
        changes: 1,
        keep: 1,
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
    engine.backup_tick()?;
    assert!(latest(&a)?.is_empty());
    let files = latest(&b)?;
    assert_eq!(files.len(), 1);
    assert!(
        tier::snapshot::load(&files[0].path())?["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "documents")
    );
    Ok(())
}
