use crate::{
    config::{ArchiveBackup, ArchiveTarget, Config},
    descriptor::{ArchiveCopy, State},
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek},
    time::Duration,
};

fn request(target: &ArchiveTarget, method: &str, key: &str) -> Result<ureq::Request> {
    ensure!(
        key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid archive object key"
    );
    let mut url = url::Url::parse(&target.url)?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("invalid archive base URL"))?
        .pop_if_empty()
        .push(key);
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(300))
        .build();
    let mut request = agent.request(method, url.as_str());
    if let (Some(user), Some(password)) = (&target.username_env, &target.password_env) {
        let credentials = format!(
            "{}:{}",
            std::env::var(user).context("archive username environment missing")?,
            std::env::var(password).context("archive password environment missing")?
        );
        request = request.set(
            "Authorization",
            &format!("Basic {}", STANDARD.encode(credentials)),
        );
    }
    Ok(request)
}

fn download(config: &Config, copy: &ArchiveCopy, digest: &str, size: u64) -> Result<File> {
    let target = config
        .archives
        .get(&copy.target)
        .context("archive target unavailable")?;
    let response = request(target, "GET", &copy.key)?
        .call()
        .map_err(|_| anyhow::anyhow!("archive GET failed on {}", copy.target))?;
    let mut source = response
        .into_reader()
        .take(size.checked_add(1).context("archive size overflow")?);
    let mut file = tempfile::tempfile()?;
    let mut hash = Sha256::new();
    let mut count = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buffer[..n])?;
        hash.update(&buffer[..n]);
        count += n as u64;
    }
    ensure!(
        count == size && format!("{:x}", hash.finalize()) == digest,
        "archive content mismatch on {}",
        copy.target
    );
    file.rewind()?;
    Ok(file)
}

pub(crate) fn upload(
    config: &Config,
    backup: &ArchiveBackup,
    source: &mut File,
    digest: &str,
    size: u64,
) -> Result<Vec<ArchiveCopy>> {
    let mut uploaded = Vec::new();
    for name in &backup.targets {
        let copy = ArchiveCopy {
            target: name.clone(),
            key: digest.to_owned(),
        };
        let target = config
            .archives
            .get(name)
            .context("archive target unavailable")?;
        source.rewind()?;
        let result = request(target, "PUT", digest)?
            .set("Content-Length", &size.to_string())
            .send(&mut *source);
        if result.is_ok() && download(config, &copy, digest, size).is_ok() {
            uploaded.push(copy);
            if uploaded.len() == backup.count {
                return Ok(uploaded);
            }
        }
    }
    bail!(
        "archive backup could not create {} verified copies",
        backup.count
    )
}

pub(crate) fn read(config: &Config, state: &State) -> Result<File> {
    let digest = state
        .digest
        .as_deref()
        .context("archive has no content digest")?;
    for copy in &state.archives {
        if let Ok(file) = download(config, copy, digest, state.size) {
            return Ok(file);
        }
    }
    bail!("no verified archive copy available")
}

pub(crate) fn verify(config: &Config, state: &State) -> Result<()> {
    let digest = state
        .digest
        .as_deref()
        .context("archive has no content digest")?;
    for copy in &state.archives {
        download(config, copy, digest, state.size)?;
    }
    Ok(())
}

pub(crate) fn satisfied(backup: Option<&ArchiveBackup>, state: &State) -> bool {
    match backup {
        None => true,
        Some(backup) => {
            state
                .archives
                .iter()
                .filter(|copy| backup.targets.contains(&copy.target))
                .count()
                >= backup.count
        }
    }
}
