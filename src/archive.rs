use crate::{
    background::RateLimit,
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

#[derive(Debug)]
struct CorruptArchive;
impl std::fmt::Display for CorruptArchive {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("archive content checksum or size mismatch")
    }
}
impl std::error::Error for CorruptArchive {}

#[derive(Debug)]
struct RemoteBodyRead(std::io::Error);
impl std::fmt::Display for RemoteBodyRead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "archive response body read failed: {}", self.0)
    }
}
impl std::error::Error for RemoteBodyRead {}

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
        .timeout(Duration::from_secs(target.timeout_secs))
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
    let mut file = tempfile::tempfile()?;
    download_to(config, copy, digest, size, &mut file, None)?;
    file.rewind()?;
    Ok(file)
}

fn download_to(
    config: &Config,
    copy: &ArchiveCopy,
    digest: &str,
    size: u64,
    destination: &mut dyn std::io::Write,
    limit: Option<&RateLimit>,
) -> Result<()> {
    let _permit = permits(config, &copy.target)?;
    let target = config
        .archives
        .get(&copy.target)
        .context("archive target unavailable")?;
    ensure!(
        copy.endpoint == target.endpoint()?,
        "archive target endpoint changed"
    );
    remote_call(config, target, || {
        let response = request(target, "GET", &copy.key)?.call()?;
        let mut source = response
            .into_reader()
            .take(size.checked_add(1).context("archive size overflow")?);
        let mut hash = Sha256::new();
        let mut count = 0;
        let mut buffer = [0; 64 * 1024];
        loop {
            let n = source.read(&mut buffer).map_err(RemoteBodyRead)?;
            if n == 0 {
                break;
            }
            if let Some(limit) = limit {
                limit.consume(n);
            }
            destination.write_all(&buffer[..n])?;
            hash.update(&buffer[..n]);
            count += n as u64;
        }
        if count != size || format!("{:x}", hash.finalize()) != digest {
            return Err(CorruptArchive.into());
        }
        Ok(())
    })
}

pub(crate) fn upload(
    config: &Config,
    backup: &ArchiveBackup,
    source: &mut File,
    digest: &str,
    size: u64,
) -> Result<Vec<ArchiveCopy>> {
    let mut uploaded = Vec::new();
    let mut names: Vec<_> = config.archives.keys().collect();
    names.sort_by_cached_key(|name| {
        format!(
            "{:x}",
            Sha256::digest(format!("{digest}:{name}").as_bytes())
        )
    });
    for name in names {
        let target = &config.archives[name];
        let copy = ArchiveCopy {
            target: name.clone(),
            endpoint: target.endpoint()?,
            key: digest.to_owned(),
        };
        source.rewind()?;
        let result = (|| -> Result<()> {
            let object_key = format!(
                "object-{:x}-{digest}",
                Sha256::digest(target.endpoint()?.as_bytes())
            );
            let _object = acquire(
                config,
                &object_key,
                1,
                Duration::from_secs(target.timeout_secs),
            )?;
            match download_to(config, &copy, digest, size, &mut std::io::sink(), None) {
                Ok(_) => return Ok(()),
                Err(error)
                    if error.is::<CorruptArchive>()
                        || matches!(
                            error.downcast_ref::<ureq::Error>(),
                            Some(ureq::Error::Status(404, _))
                        ) => {}
                Err(error) => return Err(error),
            }
            {
                let _permit = permits(config, name)?;
                remote_call(config, target, || {
                    request(target, "PUT", digest)?
                        .set("Content-Length", &size.to_string())
                        .send(&mut *source)
                        .map_err(Into::into)
                })?;
            }
            download_to(config, &copy, digest, size, &mut std::io::sink(), None)?;
            Ok(())
        })();
        if result.is_ok() {
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

// A short shared cooldown prevents a failed backend from stalling every queued
// file. This is disposable scheduling state, never part of content recovery.
fn remote_call<T>(
    config: &Config,
    target: &ArchiveTarget,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let directory = config.primary.join(".tier/requests");
    std::fs::create_dir_all(&directory)?;
    let marker = directory.join(format!(
        "{:x}.retry",
        Sha256::digest(target.endpoint()?.as_bytes())
    ));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let retry = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    ensure!(now >= retry, "archive target is cooling down");
    match operation() {
        Ok(response) => Ok(response),
        Err(error) => {
            // A write failure must not conceal the original remote failure.
            if error.is::<RemoteBodyRead>()
                || error
                    .downcast_ref::<ureq::Error>()
                    .is_some_and(|error| matches!(error, ureq::Error::Transport(_)))
            {
                let failed_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let _ = std::fs::write(marker, failed_at.saturating_add(60).to_string());
            }
            Err(error)
        }
    }
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

pub(crate) fn verify(config: &Config, state: &State, limit: Option<&RateLimit>) -> Result<()> {
    let digest = state
        .digest
        .as_deref()
        .context("archive has no content digest")?;
    for copy in &state.archives {
        download_to(
            config,
            copy,
            digest,
            state.size,
            &mut std::io::sink(),
            limit,
        )?;
    }
    Ok(())
}

pub(crate) fn satisfied(config: &Config, backup: Option<&ArchiveBackup>, state: &State) -> bool {
    match backup {
        None => true,
        Some(backup) => {
            if state.archives.iter().any(|copy| {
                config
                    .archives
                    .get(&copy.target)
                    .is_none_or(|target| target.endpoint().ok().as_ref() != Some(&copy.endpoint))
            }) {
                return false;
            }
            state
                .archives
                .iter()
                .map(|copy| &copy.endpoint)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                >= backup.count
        }
    }
}

// File locks coordinate the daemon and independent Samba processes. Acquire a
// target slot before a global slot so a saturated target cannot monopolize it.
fn acquire(config: &Config, key: &str, count: usize, timeout: Duration) -> Result<File> {
    let directory = config.primary.join(".tier/requests");
    std::fs::create_dir_all(&directory)?;
    let started = std::time::Instant::now();
    loop {
        ensure!(
            started.elapsed() < timeout,
            "archive concurrency wait timed out"
        );
        for index in 0..count {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(directory.join(format!("{key}-{index}")))?;
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(file),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn permits(config: &Config, name: &str) -> Result<(File, File)> {
    let target = config
        .archives
        .get(name)
        .context("archive target unavailable")?;
    let key = format!("{:x}", Sha256::digest(target.endpoint()?.as_bytes()));
    let timeout = Duration::from_secs(target.timeout_secs);
    let target_slot = acquire(config, &key, target.concurrency, timeout)?;
    let global_slot = acquire(
        config,
        "global",
        config.background.archive_concurrency,
        timeout,
    )?;
    Ok((target_slot, global_slot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_archive_records_do_not_satisfy_multiple_copies() -> Result<()> {
        let config: Config = toml::from_str(
            "primary='/primary'\n[replicas]\n[archives.remote]\nurl='https://example.test/dav/'\n[[backup]]\npath=''\ncount=1\n",
        )?;
        let mut state = State::pending();
        let copy = ArchiveCopy {
            target: "remote".into(),
            endpoint: config.archives["remote"].endpoint()?,
            key: "0".repeat(64),
        };
        state.archives = vec![copy.clone(), copy];
        let policy = ArchiveBackup {
            count: 2,
            after_secs: 0,
            local_ttl_secs: 0,
        };
        assert!(!satisfied(&config, Some(&policy), &state));
        Ok(())
    }

    #[test]
    fn lock_acquisition_has_a_bounded_wait() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let config: Config = toml::from_str(&format!(
            "primary='{}'\n[replicas]\n[[backup]]\npath=''\ncount=1\n",
            directory.path().display()
        ))?;
        let _held = acquire(&config, "test", 1, Duration::from_secs(1))?;
        assert!(acquire(&config, "test", 1, Duration::from_millis(25)).is_err());
        Ok(())
    }

    #[test]
    fn transport_cooldown_skips_requests_and_expired_marker_retries() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        drop(listener);
        let config: Config = toml::from_str(&format!(
            "primary='{}'\n[replicas]\n[archives.remote]\nurl='http://{address}/'\n[[backup]]\npath=''\ncount=1\n",
            directory.path().display()
        ))?;
        let target = &config.archives["remote"];
        assert!(
            remote_call(&config, target, || ureq::get(&target.url)
                .call()
                .map_err(Into::into))
            .is_err()
        );
        assert!(remote_call::<()>(&config, target, || panic!("cooldown must skip HTTP")).is_err());
        let marker = directory.path().join(".tier/requests").join(format!(
            "{:x}.retry",
            Sha256::digest(target.endpoint()?.as_bytes())
        ));
        std::fs::write(marker, "0")?;
        let mut retried = false;
        assert!(
            remote_call::<()>(&config, target, || {
                retried = true;
                anyhow::bail!("test request")
            })
            .is_err()
        );
        assert!(retried);
        Ok(())
    }

    #[test]
    fn target_permit_blocks_until_previous_request_finishes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let source = format!(
            "primary='{}'\n[replicas]\n[archives.remote]\nurl='http://localhost/'\nconcurrency=1\n[[backup]]\npath=''\ncount=1\n",
            directory.path().display()
        );
        let config: Config = toml::from_str(&source)?;
        let held = permits(&config, "remote")?;
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let acquired = permits(&config, "remote").unwrap();
            sender.send(()).unwrap();
            drop(acquired);
        });
        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
        drop(held);
        receiver.recv_timeout(Duration::from_secs(2))?;
        worker.join().unwrap();
        Ok(())
    }

    #[test]
    fn interrupted_and_stalled_response_bodies_activate_cooldown() -> Result<()> {
        use std::io::Write;
        for interrupted in [true, false] {
            let directory = tempfile::tempdir()?;
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let (release, wait) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nConnection: close\r\n\r\npartial").unwrap();
                stream.flush().unwrap();
                if !interrupted {
                    let _ = wait.recv_timeout(Duration::from_secs(3));
                }
            });
            let config: Config = toml::from_str(&format!(
                "primary='{}'\n[replicas]\n[archives.remote]\nurl='http://{address}/'\ntimeout_secs=1\n[[backup]]\npath=''\ncount=1\n",
                directory.path().display()
            ))?;
            let copy = ArchiveCopy {
                target: "remote".into(),
                endpoint: config.archives["remote"].endpoint()?,
                key: "0".repeat(64),
            };
            let error = download_to(&config, &copy, &copy.key, 4096, &mut std::io::sink(), None)
                .unwrap_err();
            let _ = release.send(());
            server.join().unwrap();
            assert!(error.is::<RemoteBodyRead>(), "{error:#}");
            let retry = download_to(&config, &copy, &copy.key, 4096, &mut std::io::sink(), None)
                .unwrap_err();
            assert!(retry.to_string().contains("cooling down"), "{retry:#}");
        }
        Ok(())
    }
}
