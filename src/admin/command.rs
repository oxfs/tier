use super::Backend;
use crate::{
    archive::ArchivePolicy,
    backup::{Kind, Schedule},
    core::Policy,
    recovery,
};
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
    BackupSchedule {
        kind: Kind,
        #[serde(flatten)]
        schedule: Schedule,
    },
    Backup {
        kind: Kind,
    },
    Inspect {
        path: PathBuf,
        backup_id: Option<u32>,
        roots: Vec<PathBuf>,
    },
    Export {
        path: PathBuf,
    },
}

impl Command {
    pub(super) fn run(self, backend: &Backend) -> Result<Value> {
        let engine = &backend.engine;
        match self {
            Self::Inspect {
                path,
                backup_id,
                roots,
            } => {
                return recovery::inspect_backup(&path, backup_id, &roots, None);
            }
            Self::Repair {} => backend.maintenance.repair()?,
            Self::Export { path } => {
                ensure!(path.is_absolute(), "use an absolute export path");
                crate::export::write(engine, &path)?;
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
            Self::BackupSchedule { kind, schedule } => {
                engine.set_backup_schedule(kind, &schedule)?
            }
            Self::Backup { kind } => engine.backup_tick(kind)?,
        }
        Ok(json!({"ok":true}))
    }
}
