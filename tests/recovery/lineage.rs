use anyhow::Result;
use std::{collections::BTreeMap, fs};
use tempfile::tempdir;
use tier::{backup::Schedule, core::Engine, recovery};

#[test]
fn restored_catalog_uses_a_distinct_backup_lineage() -> Result<()> {
    let temp = tempdir()?;
    let dir = temp.path().join("snapshots");
    fs::create_dir(&dir)?;
    let source = Engine::open(&temp.path().join("source"))?;
    source.schedule(&Schedule {
        directories: vec![dir.clone()],
        seconds: 3600,
        changes: 1,
        keep: 4,
        retain_days: 0,
        hourly: 0,
        daily: 0,
    })?;
    let baseline = source.snapshot()?;
    source.mkdir("/original")?;
    let original = source.stat("/original")?.id;
    source.backup_tick()?;
    let restored = temp.path().join("restored");
    recovery::import(
        &baseline,
        &restored,
        &BTreeMap::new(),
        false,
        &Default::default(),
    )?;
    let engine = Engine::open(&restored)?;
    engine.mkdir("/replacement")?;
    let replacement = engine.stat("/replacement")?.id;
    engine.backup_tick()?;
    let mut keys = Vec::new();
    for file in fs::read_dir(dir)? {
        let path = file?.path();
        if path.extension().is_some_and(|s| s == "json") {
            let data = tier::snapshot::load(&path)?;
            keys.extend(
                data["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r["key"].as_str().unwrap().to_owned()),
            );
        }
    }
    assert!(keys.contains(&format!("node/{original}")));
    assert!(keys.contains(&format!("node/{replacement}")));
    Ok(())
}
