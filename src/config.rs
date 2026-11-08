use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    env, fs,
    io::ErrorKind,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    db: Option<PathBuf>,
    pub mountpoint: Option<PathBuf>,
    pub admin: Option<SocketAddr>,
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
        for path in settings.db.iter().chain(&settings.mountpoint) {
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
        Ok(dir.join("tier.sqlite"))
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
            "db='/srv/catalog'\nmountpoint='/srv/share'\nadmin='127.0.0.1:9840'",
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
}
