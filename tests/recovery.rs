use anyhow::Result;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};
use tier::{Store, config::Config, recovery::recover_primary};

#[test]
fn primary_recovery_restores_verified_snapshot_and_refuses_nonempty_target() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let primary = temp.path().join("primary");
    let replica = temp.path().join("replica");
    let recovered = temp.path().join("recovered");
    for root in [&primary, &replica, &recovered] {
        fs::create_dir(root)?;
    }
    let config = |root: &Path| -> Result<Config> {
        Ok(toml::from_str(&format!(
            "primary={:?}\n[replicas]\na={:?}\n[[backup]]\npath=''\ncount=2\n",
            root, replica
        ))?)
    };
    let store = Store::initialize(config(&primary)?)?;
    {
        let mut file = store.write(Path::new("photo.jpg"))?;
        file.write_all(b"original photo")?;
    }
    store.reconcile(Path::new("photo.jpg"))?;
    assert_eq!(recover_primary(config(&recovered)?)?, 1);
    let restored = Store::new(config(&recovered)?)?;
    let mut contents = String::new();
    restored
        .read(Path::new("photo.jpg"))?
        .read_to_string(&mut contents)?;
    assert_eq!(contents, "original photo");
    assert!(recover_primary(config(&recovered)?).is_err());
    let damaged_target = temp.path().join("damaged-target");
    fs::create_dir(&damaged_target)?;
    fs::write(replica.join("photo.jpg"), b"corruption")?;
    assert!(recover_primary(config(&damaged_target)?).is_err());
    assert!(!damaged_target.join(".tier/storage.json").exists());
    Ok(())
}

#[test]
fn recovery_rejects_internal_metadata_symlink_without_touching_target() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let primary = temp.path().join("primary");
    let replica = temp.path().join("replica");
    let target = temp.path().join("target");
    for root in [&primary, &replica, &target] {
        fs::create_dir(root)?;
    }
    let config = |primary: &Path| -> Result<Config> {
        Ok(toml::from_str(&format!(
            "primary={primary:?}\n[replicas]\na={replica:?}\n[[backup]]\npath=''\ncount=2\n"
        ))?)
    };
    let store = Store::initialize(config(&primary)?)?;
    store.write(Path::new("photo"))?.write_all(b"photo")?;
    store.reconcile(Path::new("photo"))?;
    let moved = temp.path().join("outside-meta");
    fs::rename(replica.join(".tier/meta"), &moved)?;
    std::os::unix::fs::symlink(&moved, replica.join(".tier/meta"))?;
    assert!(recover_primary(config(&target)?).is_err());
    assert!(fs::read_dir(target)?.next().is_none());
    Ok(())
}

#[test]
fn recovery_rejects_canonical_overlap_before_writing() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let replica = temp.path().join("replica");
    let target = replica.join("nested");
    fs::create_dir_all(&target)?;
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&target, &alias)?;
    let config: Config = toml::from_str(&format!(
        "primary={alias:?}\n[replicas]\na={replica:?}\n[[backup]]\npath=''\ncount=2\n"
    ))?;
    assert!(recover_primary(config).is_err());
    assert!(fs::read_dir(target)?.next().is_none());
    Ok(())
}

#[test]
fn namespace_intent_mirrors_precede_mutation_and_survive_failed_preparation() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let primary = temp.path().join("primary");
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    let recovered = temp.path().join("recovered");
    for root in [&primary, &first, &second, &recovered] {
        fs::create_dir(root)?;
    }
    let config = |primary: &Path| -> Result<Config> {
        Ok(toml::from_str(&format!(
            "primary={primary:?}\n[replicas]\na={first:?}\nb={second:?}\n[[backup]]\npath=''\ncount=3\n"
        ))?)
    };
    let store = Store::initialize(config(&primary)?)?;
    store.write(Path::new("source"))?.write_all(b"original")?;
    store
        .write(Path::new("destination"))?
        .write_all(b"keep until intent durable")?;
    store.reconcile(Path::new("source"))?;
    store.reconcile(Path::new("destination"))?;
    let blocker = second.join(".tier/namespace.json");
    fs::create_dir(&blocker)?;
    assert!(
        store
            .rename(Path::new("source"), Path::new("destination"))
            .is_err()
    );
    for root in [&primary, &first, &second] {
        assert_eq!(fs::read(root.join("source"))?, b"original");
        assert_eq!(
            fs::read(root.join("destination"))?,
            b"keep until intent durable"
        );
    }
    assert!(first.join(".tier/namespace.json").is_file());
    assert!(recover_primary(config(&recovered)?).is_err());
    assert!(fs::read_dir(&recovered)?.next().is_none());
    fs::remove_dir(blocker)?;
    Store::new(config(&primary)?)?;
    for root in [&primary, &first, &second] {
        assert!(!root.join("source").exists());
        assert_eq!(fs::read(root.join("destination"))?, b"original");
        assert!(!root.join(".tier/namespace.json").exists());
    }
    Ok(())
}

#[test]
fn recovery_refuses_disagreement_and_pending_survivor_descriptors() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let primary = temp.path().join("primary");
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    let target = temp.path().join("target");
    for root in [&primary, &first, &second, &target] {
        fs::create_dir(root)?;
    }
    let config = |primary: &Path| -> Result<Config> {
        Ok(toml::from_str(&format!(
            "primary={primary:?}\n[replicas]\na={first:?}\nb={second:?}\n[[backup]]\npath=''\ncount=3\n"
        ))?)
    };
    let store = Store::initialize(config(&primary)?)?;
    store.write(Path::new("photo"))?.write_all(b"first")?;
    store.reconcile(Path::new("photo"))?;
    let old = fs::read(second.join(".tier/meta/photo"))?;
    store.write(Path::new("photo"))?.write_all(b"other")?;
    let pending = fs::read(primary.join(".tier/meta/photo"))?;
    store.reconcile(Path::new("photo"))?;
    fs::write(second.join(".tier/meta/photo"), old)?;
    assert!(recover_primary(config(&target)?).is_err());
    assert!(fs::read_dir(&target)?.next().is_none());
    for root in [&first, &second] {
        fs::write(root.join(".tier/meta/photo"), &pending)?;
    }
    assert!(recover_primary(config(&target)?).is_err());
    assert!(fs::read_dir(&target)?.next().is_none());
    Ok(())
}
