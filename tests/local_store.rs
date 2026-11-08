use std::fs;
use tempfile::tempdir;
use tier::core::{Engine, Policy};

#[test]
fn local_replication_remains_readable_when_front_is_offline() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let db = Engine::open(&tmp.path().join("tier.db"))?;
    for target in ["ssd", "hdd1", "hdd2"] {
        let path = tmp.path().join(target);
        fs::create_dir(&path)?;
        db.register_target(target, &path, None, true)?;
    }
    db.set_policy(
        "/",
        &Policy {
            front: vec!["ssd".into()],
            long: vec!["hdd1".into(), "hdd2".into()],
            long_count: 2,
            ..Policy::default()
        },
    )?;
    db.mkdir("/photos")?;
    db.put("/photos/a", b"old")?;
    db.put("/photos/a", b"current")?;
    db.reconcile()?;
    fs::rename(tmp.path().join("ssd"), tmp.path().join("ssd.offline"))?;
    assert_eq!(db.get("/PHOTOS/a")?, b"current");
    db.rename("/photos/a", "/photos/b")?;
    db.remove("/photos/b")?;
    db.restore(1, "/photos/c")?;
    assert_eq!(db.get("/photos/c")?, b"current");
    db.remove("/photos/c")?;
    assert!(db.purge(1).is_err());
    db.purge(2)?;
    assert!(db.list("/photos")?.is_empty());
    Ok(())
}

#[test]
fn replacing_policy_copies_before_discarding_the_last_readable_target() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let engine = Engine::open(&temp.path().join("tier.db"))?;
    for name in ["old", "new"] {
        let path = temp.path().join(name);
        fs::create_dir(&path)?;
        engine.register_target(name, &path, None, true)?;
    }
    engine.set_policy(
        "/",
        &Policy {
            front: vec!["old".into()],
            ..Policy::default()
        },
    )?;
    engine.put("/book", b"safe")?;
    engine.set_policy(
        "/",
        &Policy {
            front: vec!["new".into()],
            ..Policy::default()
        },
    )?;
    fs::rename(temp.path().join("new"), temp.path().join("offline"))?;
    assert!(engine.automatic_reconcile().is_err());
    assert_eq!(engine.get("/book")?, b"safe");
    fs::rename(temp.path().join("offline"), temp.path().join("new"))?;
    engine.automatic_reconcile()?;
    fs::rename(temp.path().join("old"), temp.path().join("offline"))?;
    assert_eq!(engine.get("/book")?, b"safe");
    let snapshot = engine.snapshot()?;
    let copies = snapshot["copies"].as_array().unwrap();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0]["target"], "new");
    Ok(())
}

#[test]
fn pending_writes_survive_restart_and_block_replication() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let db = tmp.path().join("tier.db");
    let engine = Engine::open(&db)?;
    for name in ["a", "b"] {
        let path = tmp.path().join(name);
        fs::create_dir(&path)?;
        engine.register_target(name, &path, None, true)?;
    }
    engine.set_policy(
        "/",
        &Policy {
            front: vec!["a".into()],
            long: vec!["b".into()],
            long_count: 1,
            ..Policy::default()
        },
    )?;
    engine.put("/data", b"first contents")?;
    engine.begin_write("/data")?;
    drop(engine);
    let engine = Engine::open(&db)?;
    engine.reconcile()?;
    assert_eq!(engine.state()?["health"][0]["status"], "unconfirmed");
    assert_eq!(engine.snapshot()?["copies"].as_array().unwrap().len(), 1);
    engine.remove("/data")?;
    engine.put("/data", b"replacement")?;
    engine.reconcile()?;
    assert_eq!(engine.state()?["health"][0]["status"], "healthy");
    engine.purge(1)?;
    Ok(())
}

#[test]
fn manual_mode_waits_for_operator_then_replaces_a_corrupt_copy() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let engine = Engine::open(&tmp.path().join("tier.db"))?;
    for name in ["front", "long"] {
        let path = tmp.path().join(name);
        fs::create_dir(&path)?;
        engine.register_target(name, &path, None, true)?;
    }
    engine.set_policy(
        "/",
        &Policy {
            front: vec!["front".into()],
            long: vec!["long".into()],
            long_count: 1,
            manual_repair: true,
            ..Policy::default()
        },
    )?;
    engine.put("/x", b"verified")?;
    engine.automatic_reconcile()?;
    let snapshot = engine.snapshot()?;
    let object = snapshot["copies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["target"] == "long")
        .unwrap()["object"]
        .as_str()
        .unwrap();
    fs::remove_file(tmp.path().join("long").join(object))?;
    assert!(engine.automatic_reconcile().is_err());
    assert!(!tmp.path().join("long").join(object).exists());
    engine.reconcile()?;
    let snap = engine.snapshot()?;
    let replacement = snap["copies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["target"] == "long")
        .unwrap()["object"]
        .as_str()
        .unwrap();
    fs::write(tmp.path().join("long").join(replacement), b"broken")?;
    engine.reconcile()?;
    fs::rename(tmp.path().join("front"), tmp.path().join("offline"))?;
    assert_eq!(engine.get("/x")?, b"verified");
    Ok(())
}

#[test]
fn confirmation_and_direct_fallback_require_every_requested_copy() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let engine = Engine::open(&tmp.path().join("tier.db"))?;
    for name in ["cache", "a", "b"] {
        let path = tmp.path().join(name);
        fs::create_dir(&path)?;
        engine.register_target(name, &path, None, true)?;
    }
    engine.set_policy(
        "/",
        &Policy {
            front: vec!["a".into(), "b".into()],
            front_count: 2,
            ..Policy::default()
        },
    )?;
    engine.put("/pair", b"two")?;
    assert_eq!(engine.snapshot()?["copies"].as_array().unwrap().len(), 2);
    fs::rename(tmp.path().join("b"), tmp.path().join("b-offline"))?;
    assert!(engine.put("/partial", b"one").is_err());
    assert!(engine.stat("/partial").is_err());
    engine.set_policy(
        "/",
        &Policy {
            front: vec!["cache".into()],
            long: vec!["a".into(), "b".into()],
            long_count: 2,
            direct_fallback: true,
            ..Policy::default()
        },
    )?;
    fs::rename(tmp.path().join("cache"), tmp.path().join("cache-offline"))?;
    assert!(engine.put("/fallback", b"two").is_err());
    fs::rename(tmp.path().join("b-offline"), tmp.path().join("b"))?;
    engine.put("/fallback", b"two")?;
    assert_eq!(engine.get("/fallback")?, b"two");
    assert_eq!(engine.snapshot()?["copies"].as_array().unwrap().len(), 4);
    Ok(())
}
