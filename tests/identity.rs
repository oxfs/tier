use anyhow::Result;
use std::{fs, io::Write, path::Path};
use tier::{Store, config::Config};

#[test]
fn missing_replica_preserves_primary_access_but_blocks_replication() -> Result<()> {
    let root = tempfile::tempdir()?;
    let primary = root.path().join("primary");
    let replica = root.path().join("replica");
    fs::create_dir(&primary)?;
    fs::create_dir(&replica)?;
    let source = format!(
        "primary={:?}\n[replicas]\nbackup={:?}\n[[backup]]\npath=''\ncount=1\ngc=true\n",
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
    assert!(store.read(Path::new("file")).is_ok());
    assert!(store.write(Path::new("file")).is_ok());
    assert!(store.list(Path::new("")).is_ok());
    assert!(store.mkdir(Path::new("new-directory")).is_ok());
    assert!(store.reconcile(Path::new("file")).is_err());
    assert_eq!(fs::read(primary.join("file"))?, b"content");
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

#[test]
fn empty_replacement_is_rebuilt_before_identity_is_published() -> Result<()> {
    let root = tempfile::tempdir()?;
    let primary = root.path().join("primary");
    let replica = root.path().join("replica");
    fs::create_dir(&primary)?;
    fs::create_dir(&replica)?;
    let source = format!(
        "primary={primary:?}\n[replicas]\nbackup={replica:?}\n[[backup]]\npath=''\ncount=2\n"
    );
    let store = Store::initialize(toml::from_str(&source)?)?;
    store
        .write(Path::new("photo"))?
        .write_all(b"photo content")?;
    store.reconcile(Path::new("photo"))?;
    fs::rename(&replica, root.path().join("old-disk"))?;
    fs::create_dir(&replica)?;
    tier::recovery::replace_replica(toml::from_str(&source)?, "backup")?;
    assert_eq!(fs::read(replica.join("photo"))?, b"photo content");
    let reopened = Store::new(toml::from_str(&source)?)?;
    assert!(reopened.check(Path::new("photo"))?);
    assert!(tier::recovery::replace_replica(toml::from_str(&source)?, "backup").is_err());
    Ok(())
}

#[test]
fn replacement_keeps_pending_file_pending_until_reconciled() -> Result<()> {
    let root = tempfile::tempdir()?;
    let primary = root.path().join("primary");
    let replica = root.path().join("replica");
    fs::create_dir(&primary)?;
    fs::create_dir(&replica)?;
    let source = format!(
        "primary={primary:?}\n[replicas]\nbackup={replica:?}\n[[backup]]\npath=''\ncount=2\n"
    );
    let store = Store::initialize(toml::from_str(&source)?)?;
    store
        .write(Path::new("new-photo"))?
        .write_all(b"new content")?;
    let registry = fs::read(primary.join(".tier/storage.json"))?;
    fs::rename(&replica, root.path().join("old-disk"))?;
    fs::create_dir(&replica)?;
    // Pending descriptors are valid, but no verified replica is claimed for them.
    tier::recovery::replace_replica(toml::from_str(&source)?, "backup")?;
    assert!(!replica.join("new-photo").exists());
    assert_ne!(fs::read(primary.join(".tier/storage.json"))?, registry);
    let reopened = Store::new(toml::from_str(&source)?)?;
    reopened.reconcile(Path::new("new-photo"))?;
    assert_eq!(fs::read(replica.join("new-photo"))?, b"new content");
    Ok(())
}

#[test]
fn replacement_refuses_corrupt_only_source_without_publishing_identity() -> Result<()> {
    let root = tempfile::tempdir()?;
    let primary = root.path().join("primary");
    let replica = root.path().join("replica");
    fs::create_dir(&primary)?;
    fs::create_dir(&replica)?;
    let source = format!(
        "primary={primary:?}\n[replicas]\nbackup={replica:?}\n[[backup]]\npath=''\ncount=2\n"
    );
    let store = Store::initialize(toml::from_str(&source)?)?;
    store.write(Path::new("photo"))?.write_all(b"valid")?;
    store.reconcile(Path::new("photo"))?;
    let registry = fs::read(primary.join(".tier/storage.json"))?;
    fs::rename(&replica, root.path().join("old-disk"))?;
    fs::create_dir(&replica)?;
    fs::write(primary.join("photo"), b"corrupt")?;
    assert!(tier::recovery::replace_replica(toml::from_str(&source)?, "backup").is_err());
    assert_eq!(fs::read(primary.join(".tier/storage.json"))?, registry);
    assert!(!replica.join(".tier/disk-id").exists());
    Ok(())
}
