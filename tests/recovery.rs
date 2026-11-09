use anyhow::Result;
use std::{collections::BTreeMap, fs, path::Path};
use tempfile::tempdir;
use tier::{
    core::{Engine, Policy},
    recovery,
};

fn setup(root: &Path) -> Result<(Engine, std::path::PathBuf)> {
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
fn object(disk: &Path, id: &str) -> Result<std::path::PathBuf> {
    Ok(fs::read_dir(disk.join("objects").join(id))?
        .next()
        .unwrap()?
        .path())
}

#[test]
fn damaged_media_requires_explicit_degraded_restore() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, disk) = setup(tmp.path())?;
    engine.put("/photo", b"correct")?;
    engine.reconcile()?;
    let snapshot = engine.snapshot()?;
    let file = object(&disk, &engine.stat("/photo")?.id)?;
    fs::write(&file, b"corrupt")?;
    let destination = tmp.path().join("recovered");
    assert!(recovery::import(&snapshot, &destination, &BTreeMap::new(), false).is_err());
    assert!(!destination.exists());
    let report = recovery::import(&snapshot, &destination, &BTreeMap::new(), true)?;
    assert_eq!(report["missing"], serde_json::json!(["photo"]));
    assert_eq!(report["damaged"], serde_json::json!([file]));
    assert!(Engine::open(&destination)?.get("/photo").is_err());
    Ok(())
}

#[test]
fn trash_is_verified_but_not_extracted_into_live_namespace() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, disk) = setup(tmp.path())?;
    engine.mkdir("/old")?;
    engine.put("/old/photo", b"photo")?;
    engine.put("/live", b"live")?;
    engine.reconcile()?;
    engine.remove("/old")?;
    let snapshot = engine.snapshot()?;
    let out = tmp.path().join("extracted");
    let report = recovery::inspect(&snapshot, &[disk], Some(&out))?;
    assert_eq!(
        report["trash"]["verified"],
        serde_json::json!([{"entry":1,"path":"old/photo"}])
    );
    assert!(!out.join("old").exists());
    assert_eq!(fs::read(out.join("live"))?, b"live");
    let restored = tmp.path().join("restored");
    recovery::import(&snapshot, &restored, &BTreeMap::new(), false)?;
    let engine = Engine::open(&restored)?;
    engine.restore(1, "/restored")?;
    assert_eq!(engine.get("/restored/photo")?, b"photo");
    Ok(())
}

#[test]
fn snapshots_reject_duplicates_and_checksum_changes() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, _) = setup(tmp.path())?;
    let mut image = engine.snapshot()?;
    let record = image["records"][0].clone();
    image["records"].as_array_mut().unwrap().push(record);
    assert!(recovery::preflight(&image, &[]).is_err());
    let path = tmp.path().join("snapshot.json");
    tier::snapshot::write(&engine, &path)?;
    let mut envelope: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    envelope["metadata"]["seq"] = 999.into();
    fs::write(&path, serde_json::to_vec(&envelope)?)?;
    assert!(tier::snapshot::load(&path).is_err());
    Ok(())
}
