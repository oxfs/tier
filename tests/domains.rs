use anyhow::Result;
use sha2::{Digest, Sha256};
use std::fs;
use tier::core::{Engine, Policy};
#[test]
fn background_placement_prefers_independent_failure_domains() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let e = Engine::open(&tmp.path().join("db"))?;
    let seed = tmp.path().join("cache");
    fs::create_dir(&seed)?;
    e.add_target("cache", &seed)?;
    e.set_policy(
        "/",
        &Policy {
            write_cache: vec!["cache".into()],
            low_water: 99,
            high_water: 100,
            ..Default::default()
        },
    )?;
    e.put("/photo", b"data")?;
    let id = e.stat("/photo")?.id;
    let mut names: Vec<_> = (0..3).map(|n| format!("disk{n}")).collect();
    names.sort_by_cached_key(|n| Sha256::digest(format!("{id}/{n}")).to_vec());
    for (i, n) in names.iter().enumerate() {
        let path = tmp.path().join(n);
        fs::create_dir(&path)?;
        e.register_target(n, &path, Some(if i < 2 { "A" } else { "B" }), true)?;
    }
    e.set_policy(
        "/",
        &Policy {
            write_cache: vec!["cache".into()],
            persistent_storage: names.clone(),
            persistent_copies: 2,
            low_water: 99,
            high_water: 100,
            ..Default::default()
        },
    )?;
    e.reconcile()?;
    assert_eq!(e.state()?["health"][0]["current"], 2);
    assert!(tmp.path().join(&names[2]).join("objects").join(id).is_dir());
    Ok(())
}
#[test]
fn rejects_overlapping_or_shared_devices_without_explicit_override() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let e = Engine::open(&tmp.path().join("db"))?;
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    fs::create_dir(&a)?;
    fs::create_dir(&b)?;
    e.add_target("a", &a)?;
    assert!(e.add_target("b", &b).is_err());
    e.register_target("b", &b, None, true)?;
    let nested = a.join("nested");
    fs::create_dir(&nested)?;
    assert!(e.register_target("nested", &nested, None, true).is_err());
    Ok(())
}
