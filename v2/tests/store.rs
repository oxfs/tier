use anyhow::Result;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use tier_v2::{
    Store,
    config::{Config, Policy},
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
            primary: self.primary.clone(),
            replicas: BTreeMap::from([
                ("a".into(), self.replicas[0].clone()),
                ("b".into(), self.replicas[1].clone()),
            ]),
            policies: vec![Policy {
                path: "".into(),
                copies: 2,
                gc,
            }],
        }
    }

    fn store(&self, gc: bool) -> Result<Store> {
        Store::new(self.config(gc))
    }
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
fn gc_preserves_reads_and_later_writes_restore_primary() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", b"before")?;
    assert!(store.reconcile(Path::new("file"))?);
    assert!(!fixture.primary.join("file").exists());
    assert_eq!(read(&store, "file")?, b"before");
    write(&store, "file", b"after!")?;
    assert!(fixture.primary.join("file").is_file());
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
fn corrupt_replica_is_reported_and_reads_use_a_verified_copy() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", b"verified")?;
    assert!(store.reconcile(Path::new("file"))?);
    fs::write(fixture.replicas[0].join("file"), b"corrupt!")?;
    assert!(store.check(Path::new("file")).is_err());
    assert_eq!(read(&store, "file")?, b"verified");
    fs::write(fixture.replicas[1].join("file"), b"corrupt!")?;
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
fn primary_only_policy_restores_garbage_collected_content() -> Result<()> {
    let fixture = Fixture::new()?;
    let store = fixture.store(true)?;
    write(&store, "file", b"retained")?;
    store.reconcile(Path::new("file"))?;
    assert!(!fixture.primary.join("file").exists());
    let mut config = fixture.config(false);
    config.policies[0].copies = 0;
    let store = Store::new(config)?;
    assert!(store.scan()?.errors.is_empty());
    assert_eq!(fs::read(fixture.primary.join("file"))?, b"retained");
    assert_eq!(read(&store, "file")?, b"retained");
    assert!(store.check(Path::new("file"))?);
    Ok(())
}

#[test]
fn inspection_detects_primary_corruption_and_missing_only_copy() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut config = fixture.config(false);
    config.policies[0].copies = 0;
    let store = Store::new(config)?;
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
fn cli_writes_truncate_then_scan_read_and_check() -> Result<()> {
    use std::process::{Command, Stdio};
    let fixture = Fixture::new()?;
    let config = fixture._directory.path().join("config.toml");
    fs::write(
        &config,
        format!(
            "primary = {:?}\n[replicas]\na = {:?}\nb = {:?}\n[[policies]]\npath = ''\ncopies = 2\ngc = true\n",
            fixture.primary, fixture.replicas[0], fixture.replicas[1]
        ),
    )?;
    let command = |operation: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tier-v2"));
        command.arg(&config).arg(operation);
        command
    };
    for bytes in [b"long contents".as_slice(), b"short".as_slice()] {
        let mut child = command("write")
            .arg("file")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(bytes)?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let scan = command("scan").output()?;
    assert!(
        scan.status.success(),
        "{}",
        String::from_utf8_lossy(&scan.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&scan.stdout)?;
    assert_eq!(report["completed"], 1);
    let output = command("read").arg("file").output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"short");
    let check = command("check").arg("file").output()?;
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    Ok(())
}
