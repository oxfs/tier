use anyhow::Result;
use std::{env, fs, path::PathBuf};
use tier::core::{Engine, Policy};

/// TIER_TEST_CACHE must be a dedicated empty 8 MiB filesystem.
#[test]
#[ignore = "requires a dedicated 8 MiB tmpfs"]
fn full_cache_respects_the_configured_confirmation_policy() -> Result<()> {
    let cache = PathBuf::from(env::var("TIER_TEST_CACHE")?);
    let work = tempfile::tempdir()?;
    let db = Engine::open(&work.path().join("tier.db"))?;
    db.add_target("cache", &cache)?;
    let disk = work.path().join("disk");
    fs::create_dir(&disk)?;
    db.add_target("disk", &disk)?;
    let mut policy = Policy {
        write_cache: vec!["cache".into()],
        persistent_storage: vec!["disk".into()],
        persistent_copies: 1,
        low_water: 85,
        high_water: 90,
        ..Policy::default()
    };
    db.set_policy("/", &policy)?;
    let blob = vec![1; 5 * 1024 * 1024];
    db.put("/first", &blob)?;
    assert!(db.put("/second", &blob).is_err());
    assert_eq!(db.get("/first")?, blob);
    policy.direct_fallback = true;
    db.set_policy("/", &policy)?;
    db.put("/second", &blob)?;
    assert_eq!(db.get("/second")?, blob);
    db.reconcile()?;
    db.state()?;
    policy.direct_fallback = false;
    db.set_policy("/", &policy)?;
    db.put("/third", &blob)?;
    let third = db.stat("/third")?.id;
    assert!(
        db.snapshot()?["copies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["node"] == third && row["target"] == "cache")
    );
    assert_eq!(db.get("/first")?, blob);
    assert_eq!(db.get("/third")?, blob);
    policy.direct_fallback = true;
    policy.write_cache = vec!["disk".into()];
    policy.persistent_storage = vec!["cache".into()];
    db.set_policy("/", &policy)?;
    fs::rename(&disk, work.path().join("offline"))?;
    let error = db.put("/oversized", &vec![0; 9 * 1024 * 1024]).unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error),
        Some(libc::ENOSPC)
    );
    assert!(db.stat("/oversized").is_err());
    fs::rename(work.path().join("offline"), &disk)?;
    assert_eq!(db.get("/third")?, blob);
    Ok(())
}
