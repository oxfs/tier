use anyhow::Result;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};
use tier::{Store, config::Config, recovery::recover_primary};

#[test]
fn designated_copy_survives_two_primary_recoveries_and_further_writes() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let roots: Vec<_> = ["primary", "a", "b", "recovered", "again"]
        .into_iter()
        .map(|name| temp.path().join(name))
        .collect();
    for root in &roots {
        fs::create_dir(root)?;
    }
    let config = |primary: &Path| -> Result<Config> {
        Ok(toml::from_str(&format!(
            "primary={primary:?}\n[replicas]\na={:?}\nb={:?}\n[[backup]]\npath=''\ncount=2\ngc=true\n",
            roots[1], roots[2]
        ))?)
    };
    let path = Path::new("photo");
    let store = Store::initialize(config(&roots[0])?)?;
    store.write(path)?.write_all(b"first")?;
    store.reconcile(path)?;
    assert!(!roots[0].join(path).exists());
    store.write(path)?.write_all(b"newer")?;
    assert!(!roots[0].join(path).exists());
    store.reconcile(path)?;
    drop(store);
    for (root, expected, updated) in [
        (&roots[3], b"newer", b"third"),
        (&roots[4], b"third", b"final"),
    ] {
        assert_eq!(recover_primary(config(root)?)?, 1);
        let restored = Store::new(config(root)?)?;
        let mut bytes = Vec::new();
        restored.read(path)?.read_to_end(&mut bytes)?;
        assert_eq!(bytes, expected);
        restored.write(path)?.write_all(updated)?;
        restored.reconcile(path)?;
        assert!(!root.join(path).exists());
        assert!(restored.check(path)?);
        for replica in [&roots[1], &roots[2]] {
            assert_eq!(fs::read(replica.join(path))?, updated);
        }
    }
    Ok(())
}
