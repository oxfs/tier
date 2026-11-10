use anyhow::Result;
use std::fs;
use tempfile::tempdir;
use tier::core::{Engine, Policy};

#[test]
fn unicode_case_aliases_share_one_identity_and_policy_override_can_be_removed() -> Result<()> {
    let root = tempdir()?;
    let engine = Engine::open(&root.path().join("db"))?;
    for name in ["a", "b"] {
        let path = root.path().join(name);
        fs::create_dir(&path)?;
        engine.register_target(name, &path, None, true)?;
    }
    let policy = |target: &str| Policy {
        write_cache: vec![target.into()],
        ..Policy::default()
    };
    engine.set_policy("/", &policy("a"))?;
    engine.mkdir("/Été")?;
    assert!(engine.mkdir("/not\\portable").is_err());
    assert!(engine.mkdir("/ÉTÉ").is_err());
    engine.set_policy("/été", &policy("b"))?;
    engine.put("/Été/Σ", b"first")?;
    let id = engine.stat("/ÉTÉ/ς")?.id;
    engine.inherit_policy("/été", false)?;
    engine.put("/été/σ", b"second")?;
    assert_eq!(engine.stat("/Été/Σ")?.id, id);
    assert_eq!(engine.list("/été")?, ["Σ"]);
    assert!(engine.inherit_policy("/", false).is_err());
    fs::rename(root.path().join("b"), root.path().join("offline"))?;
    assert_eq!(engine.get("/été/Σ")?, b"second");
    let state = engine.state()?;
    let targets = state["targets"].as_array().unwrap();
    assert!(
        targets
            .iter()
            .any(|t| t["name"] == "a" && t["available_bytes"].as_u64().is_some())
    );
    assert!(
        targets
            .iter()
            .any(|t| t["name"] == "b" && t["total_bytes"].is_null())
    );
    Ok(())
}
