use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::tempdir;
use tier::core::{Engine, Policy};
fn objects(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = vec![];
    let dir = root.join("objects");
    if dir.exists() {
        for entry in fs::read_dir(dir)? {
            for object in fs::read_dir(entry?.path())? {
                out.push(object?.path())
            }
        }
    }
    Ok(out)
}
fn setup(names: &[&str]) -> anyhow::Result<(tempfile::TempDir, Engine)> {
    let tmp = tempdir()?;
    let db = Engine::open(&tmp.path().join("db"))?;
    for n in names {
        let p = tmp.path().join(n);
        fs::create_dir(&p)?;
        db.register_target(n, &p, Some(n), true)?;
    }
    Ok((tmp, db))
}
#[test]
fn single_target_writes_then_reconcile_survive_cache_loss() -> anyhow::Result<()> {
    let (tmp, db) = setup(&["ssd", "a", "b"])?;
    db.set_policy(
        "/",
        &Policy {
            write_cache: vec!["ssd".into()],
            persistent_storage: vec!["a".into(), "b".into()],
            persistent_copies: 2,
            ..Default::default()
        },
    )?;
    db.mkdir("/photos")?;
    db.put("/photos/a", b"old")?;
    db.put("/photos/a", b"current")?;
    assert!(objects(&tmp.path().join("a"))?.is_empty());
    assert_eq!(objects(&tmp.path().join("ssd"))?.len(), 1);
    db.reconcile()?;
    fs::rename(tmp.path().join("ssd"), tmp.path().join("offline"))?;
    assert_eq!(db.get("/PHOTOS/a")?, b"current");
    db.rename("/photos/a", "/photos/b")?;
    db.remove("/photos/b")?;
    db.restore(1, "/photos/c")?;
    assert_eq!(db.get("/photos/c")?, b"current");
    db.remove("/photos/c")?;
    db.purge(2)?;
    assert!(db.list("/photos")?.is_empty());
    Ok(())
}
#[test]
fn policy_move_keeps_old_copy_until_replacement_is_verified() -> anyhow::Result<()> {
    let (tmp, e) = setup(&["old", "new"])?;
    e.set_policy(
        "/",
        &Policy {
            write_cache: vec!["old".into()],
            ..Default::default()
        },
    )?;
    e.put("/book", b"safe")?;
    e.set_policy(
        "/",
        &Policy {
            write_cache: vec!["new".into()],
            ..Default::default()
        },
    )?;
    fs::rename(tmp.path().join("new"), tmp.path().join("offline"))?;
    assert!(e.reconcile().is_err());
    assert_eq!(e.get("/book")?, b"safe");
    fs::rename(tmp.path().join("offline"), tmp.path().join("new"))?;
    e.reconcile()?;
    assert!(objects(&tmp.path().join("old"))?.is_empty());
    assert_eq!(e.get("/book")?, b"safe");
    Ok(())
}
#[test]
fn write_handles_are_exclusive_and_reconcile_waits_until_close() -> anyhow::Result<()> {
    let (tmp, e) = setup(&["cache", "disk"])?;
    e.set_policy(
        "/",
        &Policy {
            write_cache: vec!["cache".into()],
            persistent_storage: vec!["disk".into()],
            persistent_copies: 1,
            ..Default::default()
        },
    )?;
    let h = e.open_file("/data", true, true, false)?;
    h.write_at(0, b"contents")?;
    assert!(e.open_file("/data", true, false, false).is_err());
    assert_eq!(e.get("/data")?, b"contents");
    e.reconcile()?;
    assert!(objects(&tmp.path().join("disk"))?.is_empty());
    h.sync()?;
    h.close()?;
    e.reconcile()?;
    assert_eq!(e.state()?["health"][0]["status"], "healthy");
    drop(e);
    let e = Engine::open(&tmp.path().join("db"))?;
    assert_eq!(e.get("/data")?, b"contents");
    Ok(())
}
#[test]
fn corruption_is_repaired_from_a_verified_replica_and_manual_policy_is_honored()
-> anyhow::Result<()> {
    let (tmp, e) = setup(&["cache", "disk"])?;
    e.set_policy(
        "/",
        &Policy {
            write_cache: vec!["cache".into()],
            persistent_storage: vec!["disk".into()],
            persistent_copies: 1,
            manual_repair: true,
            ..Default::default()
        },
    )?;
    e.put("/x", b"verified")?;
    e.automatic_reconcile()?;
    let bad = objects(&tmp.path().join("disk"))?.pop().unwrap();
    fs::write(&bad, b"broken")?;
    assert!(e.reconcile().is_ok());
    fs::rename(tmp.path().join("cache"), tmp.path().join("offline"))?;
    assert_eq!(e.get("/x")?, b"verified");
    Ok(())
}
#[test]
fn fallback_accepts_one_primary_even_when_eventual_replica_is_offline() -> anyhow::Result<()> {
    let (tmp, e) = setup(&["cache", "a", "b"])?;
    e.set_policy(
        "/",
        &Policy {
            write_cache: vec!["cache".into()],
            persistent_storage: vec!["a".into(), "b".into()],
            persistent_copies: 2,
            direct_fallback: true,
            ..Default::default()
        },
    )?;
    fs::rename(tmp.path().join("cache"), tmp.path().join("cache.offline"))?;
    fs::rename(tmp.path().join("b"), tmp.path().join("b.offline"))?;
    e.put("/file", b"available")?;
    assert_eq!(e.get("/file")?, b"available");
    assert!(e.reconcile().is_err());
    Ok(())
}
#[test]
fn stable_handle_observes_writes_after_rename_and_truncation() -> anyhow::Result<()> {
    let (_tmp, e) = setup(&["disk"])?;
    e.set_policy(
        "/",
        &Policy {
            write_cache: vec!["disk".into()],
            ..Default::default()
        },
    )?;
    let h = e.open_file("/a", true, true, false)?;
    h.write_at(0, b"hello")?;
    let reader = e.open_file("/a", false, false, false)?;
    e.rename("/a", "/b")?;
    h.write_at(1, b"A")?;
    h.set_len(3)?;
    let mut b = [0; 5];
    assert_eq!(reader.read_at(0, &mut b)?, 3);
    assert_eq!(&b[..3], b"hAl");
    assert_eq!(e.stat("/b")?.size, 3);
    h.sync()?;
    Ok(())
}
#[test]
fn unrelated_files_write_concurrently_without_losing_namespace_updates() -> anyhow::Result<()> {
    let (_tmp, e) = setup(&["disk"])?;
    e.set_policy(
        "/",
        &Policy {
            write_cache: vec!["disk".into()],
            ..Default::default()
        },
    )?;
    std::thread::scope(|scope| {
        let mut workers = vec![];
        for i in 0..8 {
            let e = e.clone();
            workers.push(scope.spawn(move || -> anyhow::Result<()> {
                for j in 0..16 {
                    e.put(&format!("/{i}-{j}"), &[i; 32])?;
                }
                Ok(())
            }));
        }
        for w in workers {
            w.join().unwrap()?;
        }
        Ok::<_, anyhow::Error>(())
    })?;
    assert_eq!(e.list("/")?.len(), 128);
    Ok(())
}
#[test]
fn crash_writer_child() -> anyhow::Result<()> {
    let Ok(path) = std::env::var("TIER_CRASH_TEST_DB") else {
        return Ok(());
    };
    let e = Engine::open(Path::new(&path))?;
    let h = e.open_file("/data", true, false, true)?;
    h.write_at(0, b"new durable contents")?;
    h.sync()?;
    std::process::exit(0)
}
#[test]
fn restart_never_promotes_an_old_replica_after_in_place_write() -> anyhow::Result<()> {
    let (tmp, e) = setup(&["cache", "disk"])?;
    e.set_policy(
        "/",
        &Policy {
            write_cache: vec!["cache".into()],
            persistent_storage: vec!["disk".into()],
            persistent_copies: 1,
            ..Default::default()
        },
    )?;
    e.put("/data", b"old")?;
    e.reconcile()?;
    drop(e);
    let status = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "crash_writer_child", "--nocapture"])
        .env("TIER_CRASH_TEST_DB", tmp.path().join("db"))
        .status()?;
    assert!(status.success());
    fs::rename(tmp.path().join("cache"), tmp.path().join("offline"))?;
    let e = Engine::open(&tmp.path().join("db"))?;
    assert!(
        e.get("/data").is_err(),
        "old replica must not masquerade as current"
    );
    fs::rename(tmp.path().join("offline"), tmp.path().join("cache"))?;
    assert_eq!(e.get("/data")?, b"new durable contents");
    e.reconcile()?;
    fs::rename(tmp.path().join("cache"), tmp.path().join("offline"))?;
    assert_eq!(e.get("/data")?, b"new durable contents");
    Ok(())
}
#[test]
fn empty_files_read_without_recalling_archive() -> anyhow::Result<()> {
    let (_tmp, e) = setup(&["disk"])?;
    e.set_policy(
        "/",
        &Policy {
            write_cache: vec!["disk".into()],
            ..Default::default()
        },
    )?;
    e.put("/empty", b"")?;
    assert_eq!(e.get("/empty")?, b"");
    e.reconcile()?;
    Ok(())
}

#[test]
fn initial_replication_is_not_a_repair_and_grace_defers_later_failures() -> anyhow::Result<()> {
    let (tmp, engine) = setup(&["a", "b"])?;
    let mut policy = Policy {
        persistent_storage: vec!["a".into(), "b".into()],
        persistent_copies: 2,
        manual_repair: true,
        repair_grace_secs: 3600,
        ..Default::default()
    };
    engine.set_policy("/", &policy)?;
    engine.put("/protected", b"long-term-data")?;
    engine.automatic_reconcile()?;
    assert_eq!(objects(&tmp.path().join("a"))?.len(), 1);
    let damaged = objects(&tmp.path().join("b"))?.pop().unwrap();
    fs::write(&damaged, b"broken")?;
    // Reapplying the policy schedules an immediate check without changing placement.
    policy.manual_repair = false;
    engine.set_policy("/", &policy)?;
    assert!(engine.automatic_reconcile().is_err());
    assert_eq!(fs::read(&damaged)?, b"broken");
    engine.reconcile()?;
    assert!(
        objects(&tmp.path().join("b"))?
            .iter()
            .any(|path| fs::read(path).is_ok_and(|v| v == b"long-term-data"))
    );
    Ok(())
}
