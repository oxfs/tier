use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fs};
use tier::core::{Engine, Policy};

#[test]
fn prefer_distinct_domains_for_confirmation_and_background_placement() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let engine = Engine::open(&tmp.path().join("db"))?;
    let seed = tmp.path().join("seed");
    fs::create_dir(&seed)?;
    engine.add_target("seed", &seed)?;
    engine.set_policy(
        "/",
        &Policy {
            write_cache: vec!["seed".into()],
            ..Policy::default()
        },
    )?;
    engine.put("/photo", b"first")?;
    let id = engine.stat("/photo")?.id;
    let mut pools = Vec::new();
    for prefix in ["front", "long"] {
        let mut names: Vec<_> = (0..3).map(|n| format!("{prefix}{n}")).collect();
        names.sort_by_cached_key(|name| Sha256::digest(format!("{id}/{name}")).to_vec());
        for (i, name) in names.iter().enumerate() {
            let path = tmp.path().join(name);
            fs::create_dir(&path)?;
            engine.register_target(name, &path, Some(if i < 2 { "A" } else { "B" }), true)?;
        }
        pools.push(names);
    }
    engine.set_policy(
        "/",
        &Policy {
            write_cache: pools[0].clone(),
            cache_copies: 2,
            persistent_storage: pools[1].clone(),
            persistent_copies: 2,
            ..Policy::default()
        },
    )?;
    engine.put("/photo", b"second")?;
    engine.reconcile()?;
    let snap = engine.snapshot()?;
    for pool in pools {
        let stored: HashSet<_> = snap["copies"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["target"].as_str())
            .filter(|t| pool.iter().any(|n| n == t))
            .collect();
        assert_eq!(stored.len(), 2);
        assert!(stored.contains(pool[2].as_str()));
    }
    Ok(())
}
