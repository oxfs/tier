use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    env, fs,
    io::ErrorKind,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tier::{core::RocksDbOptions, ipc::IpcOptions, service::RuntimeOptions};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    db: Option<PathBuf>,
    pub socket: Option<PathBuf>,
    pub admin: Option<SocketAddr>,
    #[serde(default)]
    pub rocksdb: RocksDbOptions,
    #[serde(default)]
    pub runtime: RuntimeOptions,
    #[serde(default)]
    pub ipc: IpcOptions,
}

fn user_dir(variable: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(path) = env::var_os(variable)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Ok(path.join("tier"));
    }
    Ok(PathBuf::from(env::var_os("HOME").context("HOME is unset")?)
        .join(fallback)
        .join("tier"))
}

impl Settings {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(path) => path.to_owned(),
            None => user_dir("XDG_CONFIG_HOME", ".config")?.join("tier.toml"),
        };
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if explicit.is_none() && error.kind() == ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let settings: Self =
            toml::from_str(&contents).with_context(|| format!("parse {}", path.display()))?;
        settings.rocksdb.validate()?;
        settings.runtime.validate()?;
        settings.ipc.validate()?;
        for path in settings.db.iter().chain(&settings.socket) {
            ensure!(
                path.is_absolute(),
                "configured path must be absolute: {}",
                path.display()
            );
        }
        Ok(settings)
    }

    pub fn database(&self, override_path: Option<PathBuf>) -> Result<PathBuf> {
        if let Some(path) = override_path.or_else(|| self.db.clone()) {
            return Ok(path);
        }
        let dir = user_dir("XDG_STATE_HOME", ".local/state")?;
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(dir.join("metadata"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bootstrap_paths_and_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tier.toml");
        assert!(Settings::load(Some(&path)).is_err());
        fs::write(
            &path,
            "db='/srv/catalog'\nsocket='/run/tier/tier.sock'\nadmin='127.0.0.1:9840'",
        )
        .unwrap();
        let config = Settings::load(Some(&path)).unwrap();
        assert_eq!(config.database(None).unwrap(), Path::new("/srv/catalog"));
        assert_eq!(
            config.database(Some("other".into())).unwrap(),
            Path::new("other")
        );
        for invalid in ["db='relative'", "policy='wrong'"] {
            fs::write(&path, invalid).unwrap();
            assert!(Settings::load(Some(&path)).is_err());
        }
    }

    #[test]
    fn runtime_defaults_overrides_and_invalid_limits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tier.toml");
        for limit in [1, 2] {
            fs::write(&path, format!("[runtime]\nmax_blocking_threads={limit}"))?;
            assert_eq!(
                Settings::load(Some(&path))?.runtime.max_blocking_threads,
                limit
            );
        }
        for invalid in [
            "max_blocking_threads=0",
            "max_blocking_threads=-1",
            "max_blocking_threads=9223372036854775807",
            "worker_threads=0",
            "worker_threads=9223372036854775807",
            "admin_request_limit=0",
            "admin_request_limit=4294967296",
            "background_task_limit=0",
            "background_task_limit=9223372036854775807",
            "workers=4",
        ] {
            fs::write(&path, format!("[runtime]\n{invalid}"))?;
            assert!(Settings::load(Some(&path)).is_err(), "accepted {invalid}");
        }
        fs::write(
            &path,
            "[runtime]\nworker_threads=1\nadmin_request_limit=3\nbackground_task_limit=2",
        )?;
        let settings = Settings::load(Some(&path))?;
        assert_eq!(settings.runtime.worker_threads, Some(1));
        assert_eq!(settings.runtime.admin_request_limit, 3);
        assert_eq!(settings.runtime.background_task_limit, 2);
        let defaults: Settings = toml::from_str(include_str!("../deploy/tier.toml"))?;
        defaults.runtime.validate()?;
        assert_eq!(defaults.runtime.worker_threads, None);
        assert_eq!(
            defaults.runtime.admin_request_limit,
            RuntimeOptions::default().admin_request_limit
        );
        assert_eq!(
            defaults.runtime.background_task_limit,
            RuntimeOptions::default().background_task_limit
        );
        assert_eq!(
            defaults.runtime.max_blocking_threads,
            RuntimeOptions::default().max_blocking_threads
        );
        Ok(())
    }

    #[test]
    fn ipc_defaults_overrides_and_invalid_limits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tier.toml");
        fs::write(
            &path,
            "[ipc]\nmax_connections=2\nmax_inflight_per_connection=1\nreply_timeout_secs=5",
        )?;
        let settings = Settings::load(Some(&path))?;
        assert_eq!(settings.ipc.max_connections, 2);
        assert_eq!(settings.ipc.max_inflight_per_connection, 1);
        assert_eq!(settings.ipc.reply_timeout_secs, 5);
        assert_eq!(settings.ipc.connection_control_buffer_mib, 16);
        for invalid in [
            "max_connections=0",
            "data_buffer_mib=1",
            "data_buffer_mib=9223372036854775807",
            "reply_timeout_secs=0",
            "connection_data_buffer_mib=256",
            "connection_control_buffer_mib=0",
            "connection_control_buffer_mib=6",
            "control_buffer_mib=4\nconnection_control_buffer_mib=8",
            "max_requests=10",
        ] {
            fs::write(&path, format!("[ipc]\n{invalid}"))?;
            assert!(Settings::load(Some(&path)).is_err(), "accepted {invalid}");
        }
        let defaults: Settings = toml::from_str(include_str!("../deploy/tier.toml"))?;
        defaults.ipc.validate()?;
        assert_eq!(
            defaults.ipc.max_connections,
            IpcOptions::default().max_connections
        );
        assert_eq!(
            defaults.ipc.max_inflight_per_connection,
            IpcOptions::default().max_inflight_per_connection
        );
        Ok(())
    }

    #[test]
    fn rocksdb_defaults_overrides_and_invalid_limits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tier.toml");
        fs::write(&path, "[rocksdb]\nblock_cache_mib=64\ncompression='none'")?;
        let settings = Settings::load(Some(&path))?;
        assert_eq!(settings.rocksdb.block_cache_mib, 64);
        assert_eq!(settings.rocksdb.write_buffer_mib, 32);
        assert!(matches!(
            settings.rocksdb.compression,
            tier::core::RocksDbCompression::None
        ));
        for invalid in [
            "block_cache_mib=0",
            "write_buffer_mib=-1",
            "write_buffer_mib=9223372036854775807",
            "max_write_buffers=1",
            "max_background_jobs=0",
            "compression='unknown'",
            "disable_wal=true",
            "block_cahce_mib=64",
        ] {
            fs::write(&path, format!("[rocksdb]\n{invalid}"))?;
            assert!(Settings::load(Some(&path)).is_err(), "accepted {invalid}");
        }
        let defaults: Settings = toml::from_str(include_str!("../deploy/tier.toml"))?;
        defaults.rocksdb.validate()?;
        assert_eq!(
            defaults.rocksdb.block_cache_mib,
            RocksDbOptions::default().block_cache_mib
        );
        Ok(())
    }
}
