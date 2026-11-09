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
    assert!(
        recovery::import(
            &snapshot,
            &destination,
            &BTreeMap::new(),
            false,
            &Default::default()
        )
        .is_err()
    );
    assert!(!destination.exists());
    let report = recovery::import(
        &snapshot,
        &destination,
        &BTreeMap::new(),
        true,
        &Default::default(),
    )?;
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
    recovery::import(
        &snapshot,
        &restored,
        &BTreeMap::new(),
        false,
        &Default::default(),
    )?;
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

#[test]
fn readable_snapshot_maps_names_and_restores_empty_directories_and_trash() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, disk) = setup(tmp.path())?;
    engine.mkdir("/empty")?;
    engine.mkdir("/old")?;
    engine.put("/old/照片.txt", b"picture")?;
    engine.put("/live", b"current")?;
    engine.reconcile()?;
    engine.remove("/old")?;
    engine.rename("/live", "/renamed")?;
    let path = tmp.path().join("readable.json");
    tier::snapshot::write(&engine, &path)?;
    let image = tier::snapshot::load(&path)?;
    let paths = image["paths"].as_array().unwrap();
    let empty = paths.iter().find(|p| p["path"] == "/empty").unwrap();
    assert_eq!(empty["kind"], "directory");
    assert_eq!(empty["copies"], serde_json::json!([]));
    let trash = paths.iter().find(|p| p["path"] == "/old/照片.txt").unwrap();
    assert_eq!(trash["namespace"], "trash");
    assert_eq!(trash["trash_id"], 1);
    let live = paths.iter().find(|p| p["path"] == "/renamed").unwrap();
    assert_eq!(live["namespace"], "live");
    assert_eq!(live["copies"][0]["current"], true);
    let physical = live["copies"][0]["physical_path"].as_str().unwrap();
    assert_eq!(fs::read(physical)?, b"current");
    assert_eq!(
        Path::new(physical),
        disk.join(live["copies"][0]["relative_path"].as_str().unwrap())
    );
    let node = image["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == format!("node/{}", live["node"].as_str().unwrap()))
        .unwrap();
    assert_eq!(node["value"]["name"], "renamed");
    assert_eq!(node["value"]["node"]["size"], 7);
    let restored = tmp.path().join("restored-readable");
    recovery::import(
        &image,
        &restored,
        &BTreeMap::new(),
        false,
        &Default::default(),
    )?;
    let reopened = Engine::open(&restored)?;
    assert!(reopened.stat("/empty")?.is_dir);
    assert_eq!(reopened.get("/renamed")?, b"current");
    reopened.restore(1, "/restored")?;
    assert_eq!(reopened.get("/restored/照片.txt")?, b"picture");
    Ok(())
}

#[test]
fn snapshot_rejects_conflicting_maps_and_unsafe_record_paths() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, _) = setup(tmp.path())?;
    engine.put("/original", b"data")?;
    engine.reconcile()?;
    let image = engine.snapshot()?;
    let mut changed = image.clone();
    changed["paths"][1]["copies"][0]["physical_path"] = "/other/file".into();
    let error = recovery::preflight(&changed, &[]).unwrap_err();
    assert!(error.to_string().contains("path mapping disagrees"));
    let mut changed = image.clone();
    let copy = changed["records"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["key"].as_str().unwrap().starts_with("copy/"))
        .unwrap();
    copy["value"]["object"] = "../../outside".into();
    assert!(recovery::preflight(&changed, &[]).is_err());
    // Record order is not part of the mapping's meaning.
    let mut reordered = image;
    reordered["records"].as_array_mut().unwrap().reverse();
    recovery::preflight(&reordered, &[])?;
    Ok(())
}

#[test]
fn older_snapshot_resolves_renamed_objects_by_identity_and_rejects_ambiguity() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, disk) = setup(tmp.path())?;
    engine.put("/before.txt", b"protected")?;
    engine.reconcile()?;
    let image = engine.snapshot()?;
    let old = object(&disk, &engine.stat("/before.txt")?.id)?;
    let id = &old.file_name().unwrap().to_str().unwrap()[..36];
    let renamed = old.with_file_name(format!("{id}--最新.txt"));
    let alias = old.with_file_name(format!("{id}--another-name.txt"));
    fs::rename(&old, &renamed)?;
    // Same UUID on unrelated inodes must not be resolved by directory order,
    // even when their contents happen to match the expected digest.
    fs::write(&alias, b"protected")?;
    let error = recovery::preflight(&image, std::slice::from_ref(&disk)).unwrap_err();
    assert!(error.to_string().contains("ambiguous renamed object"));
    fs::remove_file(&alias)?;
    fs::hard_link(&renamed, &alias)?;
    let output = tmp.path().join("extracted-renamed");
    let report = recovery::inspect(&image, std::slice::from_ref(&disk), Some(&output))?;
    assert!(report["unclaimed"].as_array().unwrap().is_empty());
    assert_eq!(fs::read(output.join("before.txt"))?, b"protected");
    let restored = tmp.path().join("restored-renamed");
    recovery::import(
        &image,
        &restored,
        &BTreeMap::new(),
        false,
        &Default::default(),
    )?;
    let reopened = Engine::open(&restored)?;
    assert_eq!(reopened.get("/before.txt")?, b"protected");
    let new_image = reopened.snapshot()?;
    let location = new_image["paths"][1]["copies"][0]["physical_path"]
        .as_str()
        .unwrap();
    assert_ne!(Path::new(location), old);
    assert!(Path::new(location).exists());
    Ok(())
}
