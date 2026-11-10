use super::{object, setup};
use anyhow::Result;
use std::{fs, path::Path};
use tempfile::tempdir;
use tier::{
    backup::{Kind, Schedule},
    core::Policy,
};

fn rows(path: &Path) -> Result<Vec<(String, String)>> {
    let mut reader = csv::Reader::from_path(path)?;
    assert_eq!(
        reader.headers()?,
        &csv::StringRecord::from(vec!["logical_path", "physical_path"])
    );
    reader
        .records()
        .map(|record| {
            let record = record?;
            Ok((record[0].to_owned(), record[1].to_owned()))
        })
        .collect()
}

#[test]
fn csv_escapes_names_and_lists_live_current_copies_and_empty_directories() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, disk) = setup(tmp.path())?;
    let second = tmp.path().join("second");
    fs::create_dir(&second)?;
    engine.register_target("second", &second, Some("other"), true)?;
    engine.set_policy(
        "/",
        &Policy {
            write_cache: vec!["disk".into()],
            persistent_storage: vec!["second".into()],
            persistent_copies: 1,
            low_water: 99,
            high_water: 100,
            ..Default::default()
        },
    )?;
    engine.mkdir("/empty")?;
    let name = "/照片,\"换行\n.txt";
    engine.put(name, b"original")?;
    engine.put("/trash", b"removed")?;
    engine.reconcile()?;
    engine.remove("/trash")?;
    let first = tmp.path().join("first.csv");
    tier::export::write(&engine, &first)?;
    let exported = rows(&first)?;
    assert!(exported.contains(&("/empty/".into(), String::new())));
    assert!(!exported.iter().any(|(path, _)| path == "/trash"));
    let copies: Vec<_> = exported.iter().filter(|(path, _)| path == name).collect();
    assert_eq!(copies.len(), 2);
    for (_, source) in copies {
        assert_eq!(fs::read(source)?, b"original");
    }
    let writer = engine.open_file(name, true, false, false)?;
    writer.write_at(0, b"modified")?;
    let next = tmp.path().join("next.csv");
    tier::export::write(&engine, &next)?;
    let exported = rows(&next)?;
    let copies: Vec<_> = exported.iter().filter(|(path, _)| path == name).collect();
    assert_eq!(copies.len(), 1, "stale replica must not appear");
    assert!(Path::new(&copies[0].1).starts_with(disk));
    writer.close()?;
    assert!(
        tier::export::write(&engine, &first).is_err(),
        "explicit export never overwrites an existing file"
    );
    Ok(())
}

#[test]
fn independent_export_schedule_records_failures_without_disabling_metadata_backups() -> Result<()> {
    let tmp = tempdir()?;
    let (engine, disk) = setup(tmp.path())?;
    engine.put("/photo", b"data")?;
    engine.reconcile()?;
    let export = tmp.path().join("exports");
    let metadata = tmp.path().join("backups");
    fs::create_dir(&export)?;
    fs::create_dir(&metadata)?;
    for (kind, directory) in [(Kind::Export, &export), (Kind::Metadata, &metadata)] {
        engine.set_backup_schedule(
            kind,
            &Schedule {
                enabled: false,
                directories: vec![directory.clone()],
                seconds: 60,
                keep: 2,
            },
        )?;
    }
    for kind in [Kind::Export, Kind::Metadata] {
        engine.schedule_backup(kind)?;
    }
    assert!(
        engine.state()?["backup_status"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    engine.backup_tick(Kind::Metadata)?;
    engine.backup_tick(Kind::Export)?;
    fs::rename(&export, tmp.path().join("detached"))?;
    assert!(engine.backup_tick(Kind::Export).is_err());
    engine.backup_tick(Kind::Metadata)?;
    let state = engine.state()?;
    let statuses = state["backup_status"].as_array().unwrap();
    assert!(statuses.iter().find(|s| s["kind"] == "export").unwrap()["error"].is_string());
    assert!(statuses.iter().find(|s| s["kind"] == "metadata").unwrap()["error"].is_null());
    assert_eq!(tier::backup::list(&metadata)?.len(), 2);
    assert!(object(&disk, &engine.stat("/photo")?.id)?.is_file());
    Ok(())
}
