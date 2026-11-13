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
    pub backup: Vec<Backup>,
    #[serde(default)]
    pub archives: BTreeMap<String, ArchiveTarget>,
    #[serde(default)]
    pub background: Background,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Background {
    pub scan_interval_secs: u64,
    pub audit_interval_secs: u64,
}
impl Default for Background {
    fn default() -> Self {
        Self {
            scan_interval_secs: 60,
            audit_interval_secs: 86400,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveTarget {
    pub url: String,
    pub username_env: Option<String>,
    pub password_env: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveBackup {
    pub targets: Vec<String>,
    pub count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    pub path: PathBuf,
    /// Required total valid content copies, including primary and archive copies.
    pub count: usize,
    #[serde(default)]
    pub gc: bool,
    pub archive: Option<ArchiveBackup>,
}

impl Backup {
    pub fn local_replicas(&self) -> usize {
        self.count
            .saturating_sub(usize::from(!self.gc))
            .saturating_sub(self.archive.as_ref().map_or(0, |archive| archive.count))
    }
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
        ensure!(
            self.background.scan_interval_secs > 0
                && self.background.audit_interval_secs >= self.background.scan_interval_secs,
            "invalid background intervals"
        );
        for target in self.archives.values() {
            let url = url::Url::parse(&target.url)?;
            ensure!(
                matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
                "invalid archive URL"
            );
            ensure!(
                url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "archive credentials must use environment variables"
            );
            ensure!(
                target.username_env.is_some() == target.password_env.is_some(),
                "archive credentials require both environment names"
            );
        }
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
            self.backup.iter().any(|p| p.path.as_os_str().is_empty()),
            "a root backup with path = \"\" is required"
        );
        for (index, backup) in self.backup.iter().enumerate() {
            if let Some(archive) = &backup.archive {
                let unique: std::collections::BTreeSet<_> = archive.targets.iter().collect();
                ensure!(
                    archive.count > 0
                        && archive.count <= unique.len()
                        && unique.len() == archive.targets.len(),
                    "invalid archive replica count or duplicate targets"
                );
                ensure!(
                    archive
                        .targets
                        .iter()
                        .all(|name| self.archives.contains_key(name)),
                    "unknown archive target"
                );
            }
            validate_relative(&backup.path)?;
            ensure!(
                backup.count > 0
                    && backup.count
                        >= usize::from(!backup.gc)
                            + backup.archive.as_ref().map_or(0, |archive| archive.count),
                "backup count must include every retained primary and archive copy"
            );
            ensure!(
                backup.local_replicas() <= self.replicas.len(),
                "backup requires more local disks than configured"
            );
            ensure!(
                !self.backup[..index].iter().any(|p| p.path == backup.path),
                "duplicate backup path: {}",
                backup.path.display()
            );
        }
        Ok(())
    }

    pub fn backup_for(&self, path: &Path) -> Result<&Backup> {
        validate_relative(path)?;
        self.backup
            .iter()
            .filter(|backup| path.starts_with(&backup.path))
            .max_by_key(|backup| backup.path.components().count())
            .context("no matching directory backup")
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
            archives: BTreeMap::new(),
            background: Background::default(),
            replicas: BTreeMap::from([("backup".into(), "/backup".into())]),
            backup: vec![Backup {
                path: "".into(),
                count: 2,
                gc: false,
                archive: None,
            }],
        }
    }

    #[test]
    fn backup_match_components_and_inherit() -> Result<()> {
        let mut config = config();
        config.backup.push(Backup {
            path: "photos".into(),
            count: 1,
            gc: true,
            archive: None,
        });
        config.validate()?;
        assert!(config.backup_for(Path::new("photos/trip/a.jpg"))?.gc);
        assert!(!config.backup_for(Path::new("photos-old/a.jpg"))?.gc);
        assert!(!config.backup_for(Path::new("a.jpg"))?.gc);
        Ok(())
    }

    #[test]
    fn rejects_unsafe_paths_and_impossible_backup() {
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
        config.backup[0].count = 3;
        assert!(config.validate().is_err());
        config.backup[0].count = 0;
        config.backup[0].gc = true;
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
    fn count_includes_primary_and_gc_requires_replacement_capacity() -> Result<()> {
        let mut config = config();
        assert_eq!(config.backup[0].local_replicas(), 1);
        config.backup[0].gc = true;
        assert_eq!(config.backup[0].local_replicas(), 2);
        assert!(config.validate().is_err());
        config.backup[0].gc = false;
        config.backup[0].count = 1;
        config.validate()?;
        assert_eq!(config.backup[0].local_replicas(), 0);
        config.backup[0].count = 0;
        assert!(config.validate().is_err());
        Ok(())
    }

    #[test]
    fn parses_explicit_root_backup_and_rejects_unknown_fields() -> Result<()> {
        let source = "primary = '/primary'\n[replicas]\nbackup = '/backup'\n[[backup]]\npath = ''\ncount = 1\n";
        let config: Config = toml::from_str(source)?;
        config.validate()?;
        assert!(!config.backup[0].gc);
        assert!(toml::from_str::<Config>(&format!("{source}unknown = true\n")).is_err());
        Ok(())
    }

    #[test]
    fn archive_backup_requires_distinct_known_targets_and_safe_urls() -> Result<()> {
        let source = "primary='/primary'\n[replicas]\n[archives.remote]\nurl='https://example.test/dav/'\n[[backup]]\npath=''\ncount=1\ngc=true\n[backup.archive]\ntargets=['remote']\ncount=1\n";
        let config: Config = toml::from_str(source)?;
        config.validate()?;
        for invalid in [
            source.replace("['remote']", "['remote','remote']"),
            source.replace("['remote']", "['missing']"),
            source.replace("https://example.test", "https://user:secret@example.test"),
            source.replace("https://example.test", "file:///tmp"),
            source.replace("count=1", "count=2"),
        ] {
            assert!(toml::from_str::<Config>(&invalid)?.validate().is_err());
        }
        Ok(())
    }
}
