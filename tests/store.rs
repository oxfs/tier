use anyhow::Result;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use tier::{
    Store,
    config::{Backup, Config},
};

struct Fixture {
    _directory: TempDir,
    primary: PathBuf,
    replicas: [PathBuf; 2],
}

impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let primary = directory.path().join("primary");
        let replicas = [
            directory.path().join("replica-a"),
            directory.path().join("replica-b"),
        ];
        for root in std::iter::once(&primary).chain(replicas.iter()) {
            fs::create_dir(root)?;
        }
        Ok(Self {
            _directory: directory,
            primary,
            replicas,
        })
    }

    fn config(&self, gc: bool) -> Config {
        Config {
            archives: Default::default(),
            background: Default::default(),
            primary: self.primary.clone(),
            replicas: BTreeMap::from([
                ("a".into(), self.replicas[0].clone()),
                ("b".into(), self.replicas[1].clone()),
            ]),
            backup: vec![Backup {
                archive: None,
                path: "".into(),
                count: 2 + usize::from(!gc),
                gc,
            }],
        }
    }

    fn store(&self, gc: bool) -> Result<Store> {
        Store::initialize(self.config(gc))
    }
}

#[test]
fn audit_workers_share_one_read_budget() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut config = fixture.config(true);
    config.background.workers = 4;
    config.background.audit_bytes_per_sec = 256 * 1024;
    let store = Store::initialize(config)?;
    for index in 0..4 {
        store
            .write(Path::new(&format!("file-{index}")))?
            .write_all(&[42; 64 * 1024])?;
    }
    assert_eq!(store.scan_pending()?.changed, 4);
    let started = std::time::Instant::now();
    let report = store.scan_local_audit()?;
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.completed, 4);
    assert!(!report.interrupted);
    // Four files with two retained copies total 512 KiB, regardless of workers.
    assert!(started.elapsed() >= std::time::Duration::from_secs(2));
    Ok(())
}

#[test]
fn returning_replica_gets_missing_metadata_without_recopying_content() -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new()?;
    let mut config = fixture.config(false);
    config.backup[0].count = 2;
    fixture.store(false)?;
    let mut offline = fixture.config(false);
    offline.backup[0].count = 2;
    offline.replicas.remove("a");
    let store = Store::new(offline)?;
    write(&store, "folder/file", b"contents")?;
    assert!(store.scan_pending()?.errors.is_empty());
    let mirror = fixture.replicas[0].join(".tier/meta/folder/file");
    assert!(!mirror.exists());
    let copy = fixture.replicas[1].join("folder/file");
    let inode = fs::metadata(&copy)?.ino();
    let store = Store::new(config)?;
    let report = store.scan_pending()?;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.changed, 1);
    assert!(mirror.is_file());
    assert_eq!(fs::metadata(copy)?.ino(), inode);
    assert!(!fixture.replicas[0].join("folder/file").exists());
    assert!(store.check(Path::new("folder/file"))?);
    assert_eq!(store.scan_pending()?.changed, 0);
    Ok(())
}

#[test]
fn missing_metadata_is_not_published_over_corrupt_content() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(false)?;
    write(&store, "file", b"contents")?;
    assert!(store.scan_pending()?.errors.is_empty());
    let mirror = fixture.replicas[0].join(".tier/meta/file");
    fs::remove_file(&mirror)?;
    fs::write(fixture.replicas[1].join("file"), b"corrupt!")?;
    let report = store.scan_pending()?;
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.changed, 0);
    assert!(!mirror.exists());
    assert_eq!(fs::read(fixture.primary.join("file"))?, b"contents");
    fs::write(fixture.replicas[1].join("file"), b"contents")?;
    let report = store.scan()?;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.changed, 1);
    assert!(store.check(Path::new("file"))?);
    Ok(())
}

#[test]
fn namespace_survives_gc_rename_and_remove() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    store.mkdir(Path::new("folder"))?;
    write(&store, "folder/file", b"contents")?;
    let id = store.logical_id(Path::new("folder/file"))?;
    store.reconcile(Path::new("folder/file"))?;
    assert_eq!(
        store.list(Path::new("folder"))?,
        vec![PathBuf::from("file")]
    );
    assert_eq!(store.stat(Path::new("folder/file"))?.0.len(), 8);
    let reader = store.read(Path::new("folder/file"))?;
    assert!(
        store
            .rename(Path::new("folder"), Path::new("moved"))
            .is_err()
    );
    drop(reader);
    store.rename(Path::new("folder"), Path::new("moved"))?;
    assert_eq!(store.logical_id(Path::new("moved/file"))?, id);
    assert_eq!(read(&store, "moved/file")?, b"contents");
    assert!(store.remove(Path::new("moved"), true).is_err());
    store.remove(Path::new("moved/file"), false)?;
    store.remove(Path::new("moved"), true)?;
    assert!(store.list(Path::new(""))?.is_empty());
    Ok(())
}

#[test]
fn exclusive_create_and_disk_identity_fail_closed() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(false)?;
    store.open_write(Path::new("file"), true, true, false)?;
    assert!(
        store
            .open_write(Path::new("file"), true, true, false)
            .is_err()
    );
    fs::rename(
        &fixture.replicas[0],
        fixture._directory.path().join("detached"),
    )?;
    fs::create_dir(&fixture.replicas[0])?;
    assert!(store.reconcile(Path::new("file")).is_err());
    assert!(fixture.store(false).is_err());
    Ok(())
}

fn write(store: &Store, path: &str, content: &[u8]) -> Result<()> {
    let mut writer = store.write(Path::new(path))?;
    writer.write_all(content)?;
    writer.sync_all()?;
    Ok(())
}

fn read(store: &Store, path: &str) -> Result<Vec<u8>> {
    let mut content = Vec::new();
    store.read(Path::new(path))?.read_to_end(&mut content)?;
    Ok(content)
}

#[test]
fn scan_copies_content_descriptors_and_empty_directories() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(false)?;
    write(&store, "photos/a.jpg", b"picture")?;
    fs::create_dir_all(fixture.primary.join("empty/nested"))?;
    assert_eq!(read(&store, "photos/a.jpg")?, b"picture");
    let report = store.scan()?;
    assert_eq!(report.completed, 1);
    assert_eq!(report.busy, 0);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let descriptor = fs::read(fixture.primary.join(".tier/meta/photos/a.jpg"))?;
    assert!(!descriptor.is_empty());
    for root in &fixture.replicas {
        assert_eq!(fs::read(root.join("photos/a.jpg"))?, b"picture");
        assert_eq!(fs::read(root.join(".tier/meta/photos/a.jpg"))?, descriptor);
        assert!(root.join("empty/nested").is_dir());
    }
    assert!(store.check(Path::new("photos/a.jpg"))?);
    Ok(())
}

#[test]
fn gc_preserves_reads_and_later_writes_modify_designated_replica() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", b"before")?;
    assert!(store.reconcile(Path::new("file"))?);
    assert!(!fixture.primary.join("file").exists());
    assert_eq!(read(&store, "file")?, b"before");
    write(&store, "file", b"after!")?;
    assert!(!fixture.primary.join("file").exists());
    assert_eq!(fs::read(fixture.replicas[0].join("file"))?, b"after!");
    assert_eq!(read(&store, "file")?, b"after!");
    assert!(store.check(Path::new("file")).is_err());
    assert!(store.reconcile(Path::new("file"))?);
    assert!(!fixture.primary.join("file").exists());
    assert_eq!(read(&store, "file")?, b"after!");
    assert!(store.check(Path::new("file"))?);
    Ok(())
}

#[test]
fn active_readers_and_writers_make_background_skip() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(false)?;
    write(&store, "file", b"contents")?;
    let reader = store.read(Path::new("file"))?;
    assert!(!store.reconcile(Path::new("file"))?);
    drop(reader);
    let writer = store.write(Path::new("file"))?;
    assert!(!store.reconcile(Path::new("file"))?);
    assert!(!store.check(Path::new("file"))?);
    drop(writer);
    assert!(store.reconcile(Path::new("file"))?);
    Ok(())
}

#[test]
fn pending_scan_counts_changes_instead_of_unchanged_files() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(false)?;
    write(&store, "file", b"contents")?;
    let held = store.read(Path::new("file"))?;
    let busy = store.scan_pending()?;
    assert_eq!((busy.completed, busy.changed, busy.busy), (0, 0, 1));
    drop(held);
    let copied = store.scan_pending()?;
    assert!(copied.errors.is_empty());
    assert_eq!((copied.completed, copied.changed, copied.busy), (1, 1, 0));
    let idle = store.scan_pending()?;
    assert!(idle.errors.is_empty());
    assert_eq!((idle.completed, idle.changed, idle.busy), (0, 0, 0));
    let audited = store.scan()?;
    assert!(audited.errors.is_empty());
    assert_eq!((audited.completed, audited.changed), (1, 0));
    write(&store, "file", b"modified")?;
    let changed = store.scan_pending()?;
    assert!(changed.errors.is_empty());
    assert_eq!((changed.completed, changed.changed), (1, 1));
    assert!(store.check(Path::new("file"))?);
    Ok(())
}

#[test]
fn move_to_cold_reuses_existing_copy_and_updates_both_after_write() -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new()?;
    let mut config = fixture.config(false);
    config.backup[0].count = 2;
    config.backup.push(Backup {
        path: "cold".into(),
        count: 2,
        gc: true,
        archive: None,
    });
    let store = Store::initialize(config)?;
    write(&store, "photos/file", b"original")?;
    assert!(store.scan_pending()?.errors.is_empty());
    let existing = fixture
        .replicas
        .iter()
        .find(|root| root.join("photos/file").exists())
        .unwrap();
    let inode = fs::metadata(existing.join("photos/file"))?.ino();
    store.mkdir(Path::new("cold"))?;
    store.rename(Path::new("photos"), Path::new("cold/photos"))?;
    let report = store.scan_pending()?;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.changed, 1);
    assert!(!fixture.primary.join("cold/photos/file").exists());
    assert_eq!(
        fs::metadata(existing.join("cold/photos/file"))?.ino(),
        inode
    );
    for root in &fixture.replicas {
        assert_eq!(fs::read(root.join("cold/photos/file"))?, b"original");
    }
    write(&store, "cold/photos/file", b"modified")?;
    assert!(store.scan_pending()?.errors.is_empty());
    for root in &fixture.replicas {
        assert_eq!(fs::read(root.join("cold/photos/file"))?, b"modified");
    }
    assert!(store.check(Path::new("cold/photos/file"))?);
    Ok(())
}

#[test]
fn gc_checks_retained_copy_before_removing_primary() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut config = fixture.config(false);
    config.backup[0].count = 2;
    let store = Store::initialize(config)?;
    write(&store, "file", b"original")?;
    store.reconcile(Path::new("file"))?;
    let existing = fixture
        .replicas
        .iter()
        .find(|root| root.join("file").exists())
        .unwrap();
    fs::write(existing.join("file"), b"corrupt!")?;
    let store = Store::new(fixture.config(true))?;
    assert!(store.reconcile(Path::new("file")).is_err());
    assert_eq!(fs::read(fixture.primary.join("file"))?, b"original");
    // A missing old copy can be recreated; no state was published after failure.
    fs::remove_file(existing.join("file"))?;
    assert!(store.reconcile(Path::new("file"))?);
    assert!(!fixture.primary.join("file").exists());
    assert!(store.check(Path::new("file"))?);
    Ok(())
}

#[test]
fn inspection_detects_corruption_but_normal_reads_do_not_hash_content() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", b"verified")?;
    assert!(store.reconcile(Path::new("file"))?);
    fs::write(fixture.replicas[0].join("file"), b"corrupt!")?;
    assert!(store.check(Path::new("file")).is_err());
    assert_eq!(read(&store, "file")?, b"corrupt!");
    fs::remove_file(fixture.replicas[0].join("file"))?;
    assert_eq!(read(&store, "file")?, b"verified");
    fs::remove_file(fixture.replicas[1].join("file"))?;
    assert!(store.read(Path::new("file")).is_err());
    Ok(())
}

#[test]
fn traversal_and_symlink_paths_are_rejected() -> Result<()> {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new()?;
    let store = fixture.store(false)?;
    for path in ["../escape", "/absolute", ".tier/meta/file"] {
        assert!(store.write(Path::new(path)).is_err(), "{path}");
        assert!(store.read(Path::new(path)).is_err(), "{path}");
    }
    symlink(&fixture.replicas[0], fixture.primary.join("escape"))?;
    assert!(store.write(Path::new("escape/file")).is_err());
    assert!(store.read(Path::new("escape/file")).is_err());
    assert!(store.scan().is_err());
    Ok(())
}

#[test]
fn roots_cannot_alias_through_a_symlink() -> Result<()> {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new()?;
    let alias = fixture._directory.path().join("alias");
    symlink(&fixture.primary, &alias)?;
    let mut config = fixture.config(false);
    config.replicas.insert("a".into(), alias);
    assert!(Store::new(config).is_err());
    Ok(())
}

#[test]
fn reducing_copy_count_retains_designated_local_primary() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", b"retained")?;
    store.reconcile(Path::new("file"))?;
    assert!(!fixture.primary.join("file").exists());
    let mut config = fixture.config(false);
    config.backup[0].count = 1;
    let store = Store::new(config)?;
    assert!(store.scan()?.errors.is_empty());
    assert!(!fixture.primary.join("file").exists());
    assert_eq!(fs::read(fixture.replicas[0].join("file"))?, b"retained");
    assert_eq!(read(&store, "file")?, b"retained");
    assert!(store.check(Path::new("file"))?);
    Ok(())
}

#[test]
fn total_backup_count_includes_primary_before_and_after_gc_changes() -> Result<()> {
    let fixture = Fixture::new()?;
    let settings = |gc| {
        let mut config = fixture.config(gc);
        config.backup[0].count = 2;
        config
    };
    let store = Store::initialize(settings(false))?;
    write(&store, "file", b"two copies")?;
    store.reconcile(Path::new("file"))?;
    assert!(fixture.primary.join("file").exists());
    assert_eq!(
        fixture
            .replicas
            .iter()
            .filter(|root| root.join("file").exists())
            .count(),
        1
    );

    let store = Store::new(settings(true))?;
    assert!(store.scan()?.errors.is_empty());
    assert!(!fixture.primary.join("file").exists());
    assert!(
        fixture
            .replicas
            .iter()
            .all(|root| root.join("file").exists())
    );

    let store = Store::new(settings(false))?;
    assert!(store.scan()?.errors.is_empty());
    assert!(!fixture.primary.join("file").exists());
    assert_eq!(read(&store, "file")?, b"two copies");
    assert!(store.check(Path::new("file"))?);
    Ok(())
}

#[test]
fn replica_stat_does_not_read_file_contents() -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", b"attributes without a content read")?;
    store.reconcile(Path::new("file"))?;
    for replica in &fixture.replicas {
        fs::File::open(replica.join("file"))?
            .set_times(fs::FileTimes::new().set_accessed(std::time::UNIX_EPOCH))?;
    }
    assert_eq!(store.stat(Path::new("file"))?.0.len(), 33);
    for replica in &fixture.replicas {
        assert_eq!(fs::metadata(replica.join("file"))?.atime(), 0);
    }
    Ok(())
}

#[test]
fn inspection_detects_primary_corruption_and_missing_only_copy() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut config = fixture.config(false);
    config.backup[0].count = 1;
    let store = Store::initialize(config)?;
    write(&store, "file", b"original")?;
    assert!(store.reconcile(Path::new("file"))?);
    assert!(store.check(Path::new("file"))?);
    fs::write(fixture.primary.join("file"), b"modified")?;
    assert!(store.check(Path::new("file")).is_err());
    fs::remove_file(fixture.primary.join("file"))?;
    assert!(store.check(Path::new("file")).is_err());
    Ok(())
}

#[test]
fn primary_descriptor_keeps_its_lock_inode_across_updates() -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", b"first")?;
    let descriptor = fixture.primary.join(".tier/meta/file");
    let inode = fs::metadata(&descriptor)?.ino();
    assert!(store.reconcile(Path::new("file"))?);
    assert_eq!(fs::metadata(&descriptor)?.ino(), inode);
    write(&store, "file", b"later")?;
    assert_eq!(fs::metadata(&descriptor)?.ino(), inode);
    assert!(store.reconcile(Path::new("file"))?);
    assert_eq!(fs::metadata(&descriptor)?.ino(), inode);
    Ok(())
}

#[test]
fn failed_descriptor_mirror_prevents_gc_and_can_be_retried() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", b"retained")?;
    let metadata = fixture.replicas[1].join(".tier/meta");
    fs::create_dir_all(metadata.parent().unwrap())?;
    fs::write(&metadata, b"blocked")?;
    assert!(store.reconcile(Path::new("file")).is_err());
    assert_eq!(fs::read(fixture.primary.join("file"))?, b"retained");
    assert_eq!(read(&store, "file")?, b"retained");
    fs::remove_file(metadata)?;
    assert!(store.reconcile(Path::new("file"))?);
    assert!(!fixture.primary.join("file").exists());
    assert!(store.check(Path::new("file"))?);
    assert_eq!(read(&store, "file")?, b"retained");
    Ok(())
}

#[test]
fn writes_truncate_then_scan_read_and_check() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    for bytes in [b"long contents".as_slice(), b"short".as_slice()] {
        let mut writer = store.open_write(Path::new("file"), true, false, true)?;
        writer.write_all(bytes)?;
        writer.sync_all()?;
    }
    let report = store.scan()?;
    assert!(report.errors.is_empty());
    assert_eq!(read(&store, "file")?, b"short");
    assert!(store.check(Path::new("file"))?);
    Ok(())
}

#[test]
fn foreground_handles_share_content_and_unrelated_renames_work() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(false)?;
    write(&store, "file", b"old")?;
    write(&store, "other", b"other")?;
    let mut writer = store.write(Path::new("file"))?;
    let mut reader = store.read(Path::new("file"))?;
    let another_writer = store.write(Path::new("file"))?;
    writer.write_all(b"new")?;
    writer.sync_all()?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"new");
    assert!(!store.check(Path::new("file"))?);
    store.rename(Path::new("other"), Path::new("renamed"))?;
    assert!(store.remove(Path::new("file"), false).is_err());
    drop((writer, reader, another_writer));
    store.reconcile(Path::new("file"))?;
    assert!(store.check(Path::new("file"))?);
    Ok(())
}

#[test]
fn rename_replaces_existing_gc_content_and_preserves_source_identity() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "source", b"new")?;
    write(&store, "destination", b"old contents")?;
    store.reconcile(Path::new("source"))?;
    let id = store.logical_id(Path::new("source"))?;
    store.rename(Path::new("source"), Path::new("destination"))?;
    assert_eq!(store.logical_id(Path::new("destination"))?, id);
    assert_eq!(read(&store, "destination")?, b"new");
    assert!(store.check(Path::new("destination"))?);
    assert!(store.read(Path::new("source")).is_err());
    assert_eq!(
        store.list(Path::new(""))?,
        vec![PathBuf::from("destination")]
    );
    Ok(())
}

#[test]
fn interrupted_rename_rolls_forward_without_deleting_already_moved_content() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "source", b"recover")?;
    store.reconcile(Path::new("source"))?;
    // Simulate a crash after the first disk's data has moved but before its
    // descriptor and the remaining disks. The prepared intent is durable.
    fs::rename(
        fixture.replicas[0].join("source"),
        fixture.replicas[0].join("destination"),
    )?;
    fs::write(
        fixture.primary.join(".tier/namespace.json"),
        serde_json::to_vec(&serde_json::json!({
            "source": "source", "destination": "destination", "directory": false, "prepared": true
        }))?,
    )?;
    assert!(store.read(Path::new("source")).is_err());
    let recovered = Store::new(fixture.config(true))?;
    assert_eq!(read(&recovered, "destination")?, b"recover");
    assert!(recovered.check(Path::new("destination"))?);
    assert!(!fixture.primary.join(".tier/namespace.json").exists());
    Ok(())
}

#[test]
fn offline_replica_blocks_cross_disk_rename_but_not_primary_reads() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(false)?;
    write(&store, "source", b"readable")?;
    fs::rename(
        &fixture.replicas[0],
        fixture._directory.path().join("detached"),
    )?;
    assert!(
        store
            .rename(Path::new("source"), Path::new("destination"))
            .is_err()
    );
    assert!(!fixture.primary.join(".tier/namespace.json").exists());
    assert_eq!(read(&store, "source")?, b"readable");
    Ok(())
}

#[test]
fn replica_and_primary_restore_preserve_permissions_and_modified_time() -> Result<()> {
    use std::{
        os::unix::fs::{MetadataExt, PermissionsExt},
        time::{Duration, UNIX_EPOCH},
    };
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", b"contents")?;
    let file = fs::File::open(fixture.primary.join("file"))?;
    file.set_permissions(fs::Permissions::from_mode(0o640))?;
    file.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(123456789)))?;
    store.reconcile(Path::new("file"))?;
    for root in &fixture.replicas {
        let metadata = fs::metadata(root.join("file"))?;
        assert_eq!(metadata.mode() & 0o777, 0o640);
        assert_eq!(metadata.mtime(), 123456789);
    }
    let restored = store.write(Path::new("file"))?;
    assert_eq!(restored.metadata()?.mode() & 0o777, 0o640);
    assert_eq!(restored.metadata()?.mtime(), 123456789);
    Ok(())
}

#[test]
fn designated_replica_readers_and_writers_share_the_same_inode() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", b"contents")?;
    store.reconcile(Path::new("file"))?;
    let mut reader = store.read(Path::new("file"))?;
    write(&store, "file", b"changed!")?;
    assert!(!store.reconcile(Path::new("file"))?);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"changed!");
    assert!(!fixture.primary.join("file").exists());
    drop(reader);
    assert!(store.reconcile(Path::new("file"))?);
    Ok(())
}

#[test]
fn staging_content_is_never_visible_in_business_directories() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", &vec![7; 8 * 1024 * 1024])?;
    let finished = Arc::new(AtomicBool::new(false));
    let stop = finished.clone();
    let roots = vec![
        fixture.primary.clone(),
        fixture.replicas[0].clone(),
        fixture.replicas[1].clone(),
    ];
    let observer = std::thread::spawn(move || -> Result<()> {
        while !stop.load(Ordering::Acquire) {
            for root in &roots {
                for entry in fs::read_dir(root)? {
                    let name = entry?.file_name();
                    assert!(
                        name == "file" || name == ".tier",
                        "visible staging file: {name:?}"
                    );
                }
            }
            std::thread::yield_now();
        }
        Ok(())
    });
    let result = (|| -> Result<()> {
        store.reconcile(Path::new("file"))?;
        store.write(Path::new("file"))?;
        Ok(())
    })();
    finished.store(true, Ordering::Release);
    observer.join().expect("directory observer")?;
    result
}

#[test]
fn busy_file_does_not_starve_other_content_audits() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(false)?;
    write(&store, "busy", b"open")?;
    write(&store, "damaged", b"original")?;
    store.scan()?;
    let _reader = store.read(Path::new("busy"))?;
    fs::write(fixture.replicas[0].join("damaged"), b"corruption")?;
    let report = store.scan_local_audit()?;
    assert!(report.busy > 0);
    assert!(report.errors.iter().any(|error| error.contains("damaged")));
    Ok(())
}

#[test]
fn already_open_primary_handles_survive_unrelated_replica_detach() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(false)?;
    write(&store, "read", b"content")?;
    let mut reader = store.read(Path::new("read"))?;
    let mut writer = store.write(Path::new("write"))?;
    fs::rename(
        &fixture.replicas[0],
        fixture._directory.path().join("offline"),
    )?;
    let mut content = Vec::new();
    reader.read_to_end(&mut content)?;
    assert_eq!(content, b"content");
    writer.write_all(b"available")?;
    writer.set_len(4)?;
    assert_eq!(fs::read(fixture.primary.join("write"))?, b"avai");
    Ok(())
}

#[test]
fn pending_designated_replica_never_falls_back_to_stale_copies() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", b"before")?;
    store.reconcile(Path::new("file"))?;
    write(&store, "file", b"after!")?;
    assert!(!fixture.primary.join("file").exists());
    assert_eq!(store.stat(Path::new("file"))?.0.len(), 6);
    let detached = fixture.replicas[0].with_extension("detached");
    fs::rename(&fixture.replicas[0], &detached)?;
    assert!(store.read(Path::new("file")).is_err());
    assert!(store.stat(Path::new("file")).is_err());
    assert!(store.write(Path::new("file")).is_err());
    fs::rename(&detached, &fixture.replicas[0])?;
    store.reconcile(Path::new("file"))?;
    assert_eq!(fs::read(fixture.replicas[1].join("file"))?, b"after!");
    assert!(!fixture.primary.join("file").exists());
    Ok(())
}
