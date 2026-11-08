use anyhow::Result;
use std::{collections::BTreeMap, fs};
use tempfile::tempdir;
use tier::{backup::Schedule, core::Engine, recovery};

#[test]
fn restore_into_the_original_backup_directory_preserves_both_histories() -> Result<()> {
    let temp = tempdir()?;
    let dir = temp.path().join("snapshots");
    fs::create_dir(&dir)?;
    let source = Engine::open(&temp.path().join("source"))?;
    source.schedule(&Schedule {
        directories: vec![dir.clone()],
        seconds: 3600,
        changes: 1,
        keep: 4,
        hourly: 0,
        daily: 0,
    })?;
    let baseline = source.snapshot()?;
    source.mkdir("/original")?;
    source.backup_tick()?;
    let restored = temp.path().join("restored");
    recovery::import(&baseline, &restored, &BTreeMap::new(), false)?;
    let engine = Engine::open(&restored)?;
    engine.mkdir("/replacement")?;
    assert_eq!(source.snapshot()?["seq"], engine.snapshot()?["seq"]);
    engine.backup_tick()?;
    let mut names = Vec::new();
    for file in fs::read_dir(dir)? {
        let path = file?.path();
        if path.extension().is_some_and(|s| s == "json") {
            let data = tier::snapshot::load(&path)?;
            names.extend(
                data["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|n| n["name"].as_str().unwrap().to_owned()),
            );
        }
    }
    assert!(
        names.iter().any(|name| name == "original")
            && names.iter().any(|name| name == "replacement")
    );
    Ok(())
}
