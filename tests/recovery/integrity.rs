use super::{backup, object, setup};
use anyhow::Result;
use std::{collections::BTreeMap, fs, os::unix::fs::symlink};
use tempfile::tempdir;
use tier::{
    backup::{self, Kind, Schedule},
    core::Engine,
    recovery,
};

#[test]
fn damaged_media_requires_explicit_degraded_restore() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, disk) = setup(tmp.path())?;
    engine.put("/photo", b"correct")?;
    engine.reconcile()?;
    let repository = backup(&engine, tmp.path())?;
    let file = object(&disk, &engine.stat("/photo")?.id)?;
    fs::write(&file, b"corrupt")?;
    let destination = tmp.path().join("recovered");
    assert!(
        recovery::restore_backup(
            &repository,
            None,
            &destination,
            &BTreeMap::new(),
            false,
            &Default::default()
        )
        .is_err()
    );
    assert!(!destination.exists());
    let report = recovery::restore_backup(
        &repository,
        None,
        &destination,
        &BTreeMap::new(),
        true,
        &Default::default(),
    )?;
    assert_eq!(report["missing"], serde_json::json!(["photo"]));
    assert_eq!(report["damaged"], serde_json::json!([file]));
    let restored = Engine::open(&destination)?;
    assert!(restored.get("/photo").is_err());
    let csv = tmp.path().join("incomplete.csv");
    assert!(tier::export::write(&restored, &csv).is_err());
    assert!(!csv.exists());
    Ok(())
}

#[test]
fn backup_restores_empty_directories_and_trash_but_disables_schedules() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, disk) = setup(tmp.path())?;
    engine.mkdir("/empty")?;
    engine.mkdir("/old")?;
    engine.put("/old/照片.txt", b"picture")?;
    engine.put("/live", b"live")?;
    engine.reconcile()?;
    engine.remove("/old")?;
    let exports = tmp.path().join("exports");
    fs::create_dir(&exports)?;
    engine.set_backup_schedule(
        Kind::Export,
        &Schedule {
            enabled: true,
            directories: vec![exports],
            seconds: 60,
            keep: 2,
        },
    )?;
    let repository = backup(&engine, tmp.path())?;
    let out = tmp.path().join("extracted");
    let report = recovery::inspect_backup(&repository, None, &[disk], Some(&out))?;
    assert_eq!(
        report["trash"]["verified"],
        serde_json::json!([{"entry":1,"path":"old/照片.txt"}])
    );
    assert!(!out.join("old").exists());
    assert!(out.join("empty").is_dir());
    assert_eq!(fs::read(out.join("live"))?, b"live");
    let restored = tmp.path().join("restored");
    let report = recovery::restore_backup(
        &repository,
        None,
        &restored,
        &BTreeMap::new(),
        false,
        &Default::default(),
    )?;
    assert_eq!(report["schedules_disabled"], true);
    let reopened = Engine::open(&restored)?;
    assert!(
        reopened.state()?["backup_schedules"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["config"]["enabled"] == false)
    );
    reopened.restore(1, "/restored")?;
    assert_eq!(reopened.get("/restored/照片.txt")?, b"picture");
    assert!(reopened.stat("/empty")?.is_dir);
    Ok(())
}

#[test]
fn older_backup_resolves_renamed_objects_but_rejects_ambiguous_inodes() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, disk) = setup(tmp.path())?;
    engine.put("/before.txt", b"protected")?;
    engine.reconcile()?;
    let repository = backup(&engine, tmp.path())?;
    let old = object(&disk, &engine.stat("/before.txt")?.id)?;
    let id = &old.file_name().unwrap().to_str().unwrap()[..36];
    let renamed = old.with_file_name(format!("{id}--最新.txt"));
    let alias = old.with_file_name(format!("{id}--another-name.txt"));
    fs::rename(&old, &renamed)?;
    fs::write(&alias, b"protected")?;
    let error =
        recovery::inspect_backup(&repository, None, std::slice::from_ref(&disk), None).unwrap_err();
    assert!(error.to_string().contains("ambiguous renamed object"));
    fs::remove_file(&alias)?;
    fs::hard_link(&renamed, &alias)?;
    let out = tmp.path().join("extracted");
    let report =
        recovery::inspect_backup(&repository, None, std::slice::from_ref(&disk), Some(&out))?;
    assert!(report["unclaimed"].as_array().unwrap().is_empty());
    assert_eq!(fs::read(out.join("before.txt"))?, b"protected");
    let restored = tmp.path().join("restored");
    recovery::restore_backup(
        &repository,
        None,
        &restored,
        &BTreeMap::new(),
        false,
        &Default::default(),
    )?;
    let reopened = Engine::open(&restored)?;
    assert_eq!(reopened.get("/before.txt")?, b"protected");
    let csv = tmp.path().join("located.csv");
    tier::export::write(&reopened, &csv)?;
    let rows = csv::Reader::from_path(csv)?
        .records()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert!(rows.iter().any(|row| row.get(1) == alias.to_str()));
    Ok(())
}

#[test]
fn missing_objects_wrong_disks_and_symlinks_do_not_bypass_recovery_checks() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, disk) = setup(tmp.path())?;
    engine.put("/photo", b"protected")?;
    engine.reconcile()?;
    let repository = backup(&engine, tmp.path())?;
    let file = object(&disk, &engine.stat("/photo")?.id)?;
    let external = tmp.path().join("outside");
    fs::rename(&file, &external)?;
    let report = recovery::inspect_backup(&repository, None, std::slice::from_ref(&disk), None)?;
    assert_eq!(report["missing"], serde_json::json!(["photo"]));
    symlink(&external, &file)?;
    assert!(
        recovery::inspect_backup(&repository, None, std::slice::from_ref(&disk), None).is_err()
    );
    fs::remove_file(file)?;
    fs::write(disk.join(".tier-id"), uuid::Uuid::new_v4().to_string())?;
    assert!(recovery::inspect_backup(&repository, None, &[disk], None).is_err());
    Ok(())
}

#[test]
fn corrupt_latest_backup_fails_without_silently_restoring_an_older_one() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, _) = setup(tmp.path())?;
    engine.put("/photo", b"protected")?;
    engine.reconcile()?;
    let repository = backup(&engine, tmp.path())?;
    engine.backup_tick(Kind::Metadata)?;
    let listings = backup::list(&repository)?;
    let older = listings[0]["id"].as_u64().unwrap() as u32;
    let newest = listings[1]["id"].as_u64().unwrap();
    fs::write(
        repository.join("meta").join(newest.to_string()),
        b"interrupted metadata",
    )?;
    let dest = tmp.path().join("restored");
    assert!(
        recovery::restore_backup(
            &repository,
            None,
            &dest,
            &BTreeMap::new(),
            false,
            &Default::default()
        )
        .is_err()
    );
    assert!(!dest.exists());
    assert_eq!(backup::list(&repository)?[1]["recognized"], false);
    recovery::restore_backup(
        &repository,
        Some(older),
        &dest,
        &BTreeMap::new(),
        false,
        &Default::default(),
    )?;
    assert_eq!(Engine::open(&dest)?.get("/photo")?, b"protected");
    assert!(
        recovery::restore_backup(
            &repository,
            Some(older),
            &repository.join("nested"),
            &BTreeMap::new(),
            false,
            &Default::default()
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn restored_metadata_discards_old_garbage_and_runtime_work() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, _) = setup(tmp.path())?;
    engine.put("/photo", b"protected")?;
    engine.reconcile()?;
    let node = engine.stat("/photo")?.id;
    drop(engine);
    // Inject a stale durable cleanup record: a recovery must not replay it.
    let database = rocksdb::DB::open_default(tmp.path().join("metadata"))?;
    let copy = database.get(format!("copy/{node}/disk"))?.unwrap();
    database.put(format!("garbage/disk/{}", uuid::Uuid::new_v4()), copy)?;
    drop(database);
    let engine = Engine::open(&tmp.path().join("metadata"))?;
    let repository = backup(&engine, tmp.path())?;
    let restored = tmp.path().join("restored");
    recovery::restore_backup(
        &repository,
        None,
        &restored,
        &BTreeMap::new(),
        false,
        &Default::default(),
    )?;
    let reopened = Engine::open(&restored)?;
    reopened.reconcile()?;
    assert_eq!(reopened.get("/photo")?, b"protected");
    drop(reopened);
    let database = rocksdb::DB::open_for_read_only(&rocksdb::Options::default(), restored, false)?;
    assert!(
        !database
            .iterator(rocksdb::IteratorMode::Start)
            .any(|entry| entry.unwrap().0.starts_with(b"garbage/"))
    );
    Ok(())
}

#[test]
fn both_incremental_repositories_restore_every_retained_backup() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, _) = setup(tmp.path())?;
    engine.put("/photo", b"unchanged body")?;
    engine.reconcile()?;
    let repositories = [tmp.path().join("a"), tmp.path().join("b")];
    for repository in &repositories {
        fs::create_dir(repository)?;
    }
    engine.set_backup_schedule(
        Kind::Metadata,
        &Schedule {
            enabled: true,
            directories: repositories.to_vec(),
            seconds: 60,
            keep: 2,
        },
    )?;
    for index in 0..4 {
        engine.mkdir(&format!("/directory-{index}"))?;
        engine.backup_tick(Kind::Metadata)?;
    }
    for (index, repository) in repositories.iter().enumerate() {
        let backups = backup::list(repository)?;
        assert_eq!(backups.len(), 2);
        for backup in backups {
            let id = backup["id"].as_u64().unwrap() as u32;
            let dest = tmp.path().join(format!("restore-{index}-{id}"));
            recovery::restore_backup(
                repository,
                Some(id),
                &dest,
                &BTreeMap::new(),
                false,
                &Default::default(),
            )?;
            let restored = Engine::open(&dest)?;
            assert_eq!(restored.get("/photo")?, b"unchanged body");
            assert!(restored.stat("/directory-2")?.is_dir);
        }
    }
    Ok(())
}

#[test]
fn unchanged_size_sst_corruption_is_detected_during_restore() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, _) = setup(tmp.path())?;
    engine.put("/photo", b"protected")?;
    engine.reconcile()?;
    let repository = backup(&engine, tmp.path())?;
    let sst = fs::read_dir(repository.join("shared_checksum"))?
        .next()
        .unwrap()?
        .path();
    let mut content = fs::read(&sst)?;
    let middle = content.len() / 2;
    content[middle] ^= 0x80;
    fs::write(sst, content)?;
    let dest = tmp.path().join("corrupt-restored");
    assert!(
        recovery::restore_backup(
            &repository,
            None,
            &dest,
            &BTreeMap::new(),
            false,
            &Default::default()
        )
        .is_err()
    );
    assert!(!dest.exists());
    Ok(())
}

#[test]
fn native_backup_rejects_unsafe_object_paths_and_invalid_target_mappings() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, disk) = setup(tmp.path())?;
    engine.put("/photo", b"protected")?;
    engine.reconcile()?;
    let node = engine.stat("/photo")?.id;
    let repository = backup(&engine, tmp.path())?;
    let unknown = BTreeMap::from([("not-registered".into(), disk.clone())]);
    let dest = tmp.path().join("wrong-map");
    assert!(
        recovery::restore_backup(
            &repository,
            None,
            &dest,
            &unknown,
            false,
            &Default::default()
        )
        .is_err()
    );
    assert!(!dest.exists());
    let replacement = tmp.path().join("replacement-disk");
    fs::create_dir(&replacement)?;
    fs::write(
        replacement.join(".tier-id"),
        uuid::Uuid::new_v4().to_string(),
    )?;
    let wrong = BTreeMap::from([("disk".into(), replacement)]);
    assert!(
        recovery::restore_backup(&repository, None, &dest, &wrong, false, &Default::default())
            .is_err()
    );
    assert!(!dest.exists());
    drop(engine);
    let database = rocksdb::DB::open_default(tmp.path().join("metadata"))?;
    let key = format!("copy/{node}/disk");
    let mut record: serde_json::Value = rmp_serde::from_slice(&database.get(&key)?.unwrap())?;
    record["object"] = "../../outside".into();
    database.put(key, rmp_serde::to_vec_named(&record)?)?;
    drop(database);
    let corrupted = Engine::open(&tmp.path().join("metadata"))?;
    corrupted.backup_tick(Kind::Metadata)?;
    let extracted = tmp.path().join("extract-unsafe");
    let error = recovery::inspect_backup(&repository, None, &[disk], Some(&extracted)).unwrap_err();
    assert!(error.to_string().contains("invalid object path"));
    assert!(!extracted.exists());
    assert!(
        recovery::restore_backup(
            &repository,
            None,
            &dest,
            &BTreeMap::new(),
            true,
            &Default::default()
        )
        .is_err()
    );
    assert!(!dest.exists());
    Ok(())
}
