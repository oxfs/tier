use anyhow::Result;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};
use tier::{Store, config::Config};

#[test]
fn missing_replica_does_not_disable_primary_reads_after_restart() -> Result<()> {
    let root = tempfile::tempdir()?;
    let primary = root.path().join("primary");
    let replica = root.path().join("replica");
    fs::create_dir(&primary)?;
    fs::create_dir(&replica)?;
    let source = format!(
        "primary={:?}\n[replicas]\nbackup={:?}\n[[backup]]\npath=''\ncount=2\n",
        primary, replica
    );
    assert!(Store::new(toml::from_str::<Config>(&source)?).is_err());
    assert!(!primary.join(".tier").exists());
    let store = Store::initialize(toml::from_str::<Config>(&source)?)?;
    {
        let mut writer = store.write(Path::new("file"))?;
        writer.write_all(b"content")?;
    }
    fs::rename(&replica, root.path().join("unplugged"))?;
    let store = Store::new(toml::from_str::<Config>(&source)?)?;
    let mut data = Vec::new();
    store.read(Path::new("file"))?.read_to_end(&mut data)?;
    assert_eq!(data, b"content");
    assert!(store.reconcile(Path::new("file")).is_err());
    // A different disk at the same mount path must not receive writes.
    fs::create_dir(&replica)?;
    assert!(store.reconcile(Path::new("file")).is_err());
    assert!(!replica.join("file").exists());
    assert!(Store::initialize(toml::from_str::<Config>(&source)?).is_err());
    assert!(!replica.join(".tier/disk-id").exists());
    fs::rename(&primary, root.path().join("lost-primary"))?;
    fs::create_dir(&primary)?;
    assert!(Store::new(toml::from_str::<Config>(&source)?).is_err());
    assert!(!primary.join(".tier").exists());
    Ok(())
}
