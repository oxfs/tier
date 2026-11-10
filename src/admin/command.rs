use super::Backend;
use crate::{archive::ArchivePolicy, backup::Schedule, core::Policy, recovery};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(tag = "action", content = "input", rename_all = "snake_case")]
pub(super) enum Command {
    Repair {},
    Restore {
        id: i64,
        path: String,
    },
    Purge {
        id: i64,
    },
    Target {
        name: String,
        path: PathBuf,
        #[serde(default)]
        domain: Option<String>,
        #[serde(default)]
        allow_same_domain: bool,
    },
    Policy {
        path: String,
        #[serde(flatten)]
        policy: Policy,
    },
    Inherit {
        path: String,
        #[serde(default)]
        archive: bool,
    },
    Archive {
        name: String,
        url: String,
        user_env: Option<String>,
        pass_env: Option<String>,
    },
    ArchivePolicy {
        path: String,
        #[serde(flatten)]
        policy: ArchivePolicy,
    },
    BackupSchedule(Schedule),
    Inspect {
        path: PathBuf,
        roots: Vec<PathBuf>,
    },
    Snapshot {
        path: PathBuf,
    },
}

impl Command {
    pub(super) fn run(self, backend: &Backend) -> Result<Value> {
        let engine = &backend.engine;
        match self {
            Self::Inspect { path, roots } => {
                return recovery::preflight(&crate::snapshot::load(&path)?, &roots);
            }
            Self::Repair {} => backend.maintenance.repair()?,
            Self::Snapshot { path } => {
                ensure!(path.is_absolute(), "use an absolute snapshot path");
                let data = engine.snapshot()?;
                crate::snapshot::publish(&data, &path)?;
            }
            Self::Restore { id, path } => engine.restore(id, &path)?,
            Self::Purge { id } => engine.purge(id)?,
            Self::Target {
                name,
                path,
                domain,
                allow_same_domain,
            } => engine.register_target(&name, &path, domain.as_deref(), allow_same_domain)?,
            Self::Policy { path, policy } => engine.set_policy(&path, &policy)?,
            Self::Inherit { path, archive } => engine.inherit_policy(&path, archive)?,
            Self::Archive {
                name,
                url,
                user_env,
                pass_env,
            } => engine.add_archive(
                &name,
                &url,
                user_env.as_deref().filter(|s| !s.is_empty()),
                pass_env.as_deref().filter(|s| !s.is_empty()),
            )?,
            Self::ArchivePolicy { path, policy } => engine.set_archive_policy(&path, &policy)?,
            Self::BackupSchedule(schedule) => engine.schedule(&schedule)?,
        }
        Ok(json!({"ok":true}))
    }
}
