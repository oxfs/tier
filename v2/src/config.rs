use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub primary: PathBuf,
    pub replicas: BTreeMap<String, PathBuf>,
    pub policies: Vec<Policy>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub path: PathBuf,
    /// Required copies on disks other than the primary.
    pub copies: usize,
    #[serde(default)]
    pub gc: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = toml::from_str(
            &fs::read_to_string(path)
                .with_context(|| format!("read configuration {}", path.display()))?,
        )?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for name in self.replicas.keys() {
            ensure!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "invalid replica name: {name}"
            );
        }
        let roots: Vec<_> = std::iter::once(&self.primary)
            .chain(self.replicas.values())
            .collect();
        for (index, root) in roots.iter().enumerate() {
            ensure!(
                root.is_absolute(),
                "storage root must be absolute: {}",
                root.display()
            );
            ensure!(
                !root
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
                "storage root must not contain dot components: {}",
                root.display()
            );
            for other in &roots[..index] {
                ensure!(
                    !root.starts_with(other) && !other.starts_with(root),
                    "storage roots overlap: {} and {}",
                    root.display(),
                    other.display()
                );
            }
        }
        ensure!(
            self.policies.iter().any(|p| p.path.as_os_str().is_empty()),
            "a root policy with path = \"\" is required"
        );
        for (index, policy) in self.policies.iter().enumerate() {
            validate_relative(&policy.path)?;
            ensure!(
                policy.copies <= self.replicas.len(),
                "policy requires more replica disks than configured"
            );
            ensure!(
                !policy.gc || policy.copies > 0,
                "GC requires at least one replica"
            );
            ensure!(
                !self.policies[..index].iter().any(|p| p.path == policy.path),
                "duplicate policy path: {}",
                policy.path.display()
            );
        }
        Ok(())
    }

    pub fn policy_for(&self, path: &Path) -> Result<&Policy> {
        validate_relative(path)?;
        self.policies
            .iter()
            .filter(|policy| path.starts_with(&policy.path))
            .max_by_key(|policy| policy.path.components().count())
            .context("no matching directory policy")
    }
}

pub fn validate_relative(path: &Path) -> Result<()> {
    for component in path.components() {
        match component {
            Component::Normal(name) if name != ".tier" => {}
            _ => bail!("unsafe relative path: {}", path.display()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            primary: "/primary".into(),
            replicas: BTreeMap::from([("backup".into(), "/backup".into())]),
            policies: vec![Policy {
                path: "".into(),
                copies: 1,
                gc: false,
            }],
        }
    }

    #[test]
    fn policies_match_components_and_inherit() -> Result<()> {
        let mut config = config();
        config.policies.push(Policy {
            path: "photos".into(),
            copies: 1,
            gc: true,
        });
        config.validate()?;
        assert!(config.policy_for(Path::new("photos/trip/a.jpg"))?.gc);
        assert!(!config.policy_for(Path::new("photos-old/a.jpg"))?.gc);
        assert!(!config.policy_for(Path::new("a.jpg"))?.gc);
        Ok(())
    }

    #[test]
    fn rejects_unsafe_paths_and_impossible_policies() {
        for path in [
            "/absolute",
            "../escape",
            "a/../b",
            ".",
            ".tier/state",
            "a/.tier/state",
        ] {
            assert!(validate_relative(Path::new(path)).is_err(), "{path}");
        }
        let mut config = config();
        config.policies[0].copies = 2;
        assert!(config.validate().is_err());
        config.policies[0].copies = 0;
        config.policies[0].gc = true;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_overlapping_roots_and_invalid_names() {
        let mut config = config();
        config
            .replicas
            .insert("backup".into(), "/primary/nested".into());
        assert!(config.validate().is_err());
        config.replicas = BTreeMap::from([("../backup".into(), "/backup".into())]);
        assert!(config.validate().is_err());
    }

    #[test]
    fn parses_explicit_root_policy_and_rejects_unknown_fields() -> Result<()> {
        let source = "primary = '/primary'\n[replicas]\nbackup = '/backup'\n[[policies]]\npath = ''\ncopies = 1\n";
        let config: Config = toml::from_str(source)?;
        config.validate()?;
        assert!(!config.policies[0].gc);
        assert!(toml::from_str::<Config>(&format!("{source}unknown = true\n")).is_err());
        Ok(())
    }
}
