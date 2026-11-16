use anyhow::Result;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
use tier::{
    Store,
    config::{ArchiveBackup, ArchiveTarget, Background, Backup, Config},
};

struct Webdav {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    puts: Arc<AtomicUsize>,
    corrupt: Arc<AtomicBool>,
}
impl Webdav {
    fn start(corrupt: bool) -> Self {
        let corrupt = Arc::new(AtomicBool::new(corrupt));
        let corruption = corrupt.clone();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", server.server_addr());
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let puts = Arc::new(AtomicUsize::new(0));
        let requests = puts.clone();
        let worker = thread::spawn(move || {
            let mut objects = BTreeMap::new();
            while !done.load(Ordering::Relaxed) {
                let Some(mut request) = server.recv_timeout(Duration::from_millis(20)).unwrap()
                else {
                    continue;
                };
                let path = request.url().to_owned();
                match request.method().as_str() {
                    "PUT" => {
                        requests.fetch_add(1, Ordering::Relaxed);
                        let mut body = Vec::new();
                        request.as_reader().read_to_end(&mut body).unwrap();
                        objects.insert(path, body);
                        request.respond(tiny_http::Response::empty(201)).unwrap();
                    }
                    "GET" => {
                        if let Some(body) = objects.get(&path) {
                            let bytes = if corruption.load(Ordering::Relaxed) {
                                b"corrupt".to_vec()
                            } else {
                                body.clone()
                            };
                            request
                                .respond(tiny_http::Response::from_data(bytes))
                                .unwrap();
                        } else {
                            request.respond(tiny_http::Response::empty(404)).unwrap();
                        }
                    }
                    _ => {
                        request.respond(tiny_http::Response::empty(405)).unwrap();
                    }
                }
            }
        });
        Self {
            url,
            stop,
            worker: Some(worker),
            puts,
            corrupt,
        }
    }
}
impl Drop for Webdav {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn config(root: &Path, servers: &[&Webdav]) -> Config {
    std::fs::create_dir_all(root.join("primary")).unwrap();
    Config {
        primary: root.join("primary"),
        replicas: (1..servers.len().max(2))
            .map(|i| {
                let path = root.join(format!("replica-{i}"));
                std::fs::create_dir_all(&path).unwrap();
                (format!("disk{i}"), path)
            })
            .collect(),
        background: Background::default(),
        archives: servers
            .iter()
            .enumerate()
            .map(|(i, s)| {
                (
                    format!("dav{i}"),
                    ArchiveTarget {
                        timeout_secs: 30,
                        concurrency: 2,
                        url: s.url.clone(),
                        username_env: None,
                        password_env: None,
                    },
                )
            })
            .collect(),
        backup: vec![Backup {
            path: "".into(),
            count: servers.len(),
            gc: true,
            archive: Some(ArchiveBackup {
                after_secs: 0,
                local_ttl_secs: 0,
                count: servers.len(),
            }),
        }],
    }
}

#[test]
fn archive_audit_limits_reads_and_still_checks_the_digest() -> Result<()> {
    let server = Webdav::start(false);
    let root = tempfile::tempdir()?;
    let mut config = config(root.path(), &[&server]);
    config.background.audit_bytes_per_sec = 128 * 1024;
    config.archives.get_mut("dav0").unwrap().timeout_secs = 1;
    let store = Store::initialize(config)?;
    store
        .write(Path::new("photo"))?
        .write_all(&[42; 256 * 1024])?;
    assert_eq!(store.scan_pending()?.changed, 1);
    assert_eq!(store.scan_pending()?.changed, 1);
    assert!(!store.root().join("photo").exists());
    let started = std::time::Instant::now();
    let report = store.scan()?;
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.completed, 1);
    assert!(started.elapsed() >= Duration::from_secs(2));
    server.corrupt.store(true, Ordering::Relaxed);
    assert_eq!(store.scan()?.errors.len(), 1);
    Ok(())
}

#[test]
fn daemon_reloads_archive_delay_and_retention_without_restart() -> Result<()> {
    use std::{fs, process::Command, time::Instant};
    struct Service(std::process::Child);
    impl Drop for Service {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn wait_until(mut condition: impl FnMut() -> bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            anyhow::ensure!(
                Instant::now() < deadline,
                "daemon did not apply archive policy"
            );
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
    let server = Webdav::start(false);
    let root = tempfile::tempdir()?;
    let config_path = root.path().join("config.toml");
    let settings = |after, ttl| {
        format!(
            "primary={:?}\n[replicas]\ndisk1={:?}\n[archives.dav0]\nurl={:?}\n[[backup]]\npath=''\ncount=1\ngc=true\narchive={{count=1,after_secs={after},local_ttl_secs={ttl}}}\n[background]\nscan_interval_secs=1\naudit_interval_secs=1\narchive_audit_interval_secs=1\n",
            root.path().join("primary"),
            root.path().join("replica-1"),
            server.url
        )
    };
    let store = Store::initialize(config(root.path(), &[&server]))?;
    {
        let mut writer = store.write(Path::new("photo"))?;
        writer.write_all(b"hot reload content")?;
    }
    fs::write(&config_path, settings(3600, 3600))?;
    let log_path = root.path().join("daemon.log");
    let mut service = Service(
        Command::new(env!("CARGO_BIN_EXE_tier"))
            .arg(&config_path)
            .env_remove("TIER_WEB_LISTEN")
            .stderr(fs::File::create(&log_path)?)
            .spawn()?,
    );
    wait_until(|| fs::read_to_string(&log_path).is_ok_and(|log| log.contains("archive_audit")))?;
    assert_eq!(server.puts.load(Ordering::Relaxed), 0);
    let replacement = root.path().join("next.toml");
    fs::write(&replacement, settings(0, 3600))?;
    fs::rename(&replacement, &config_path)?;
    wait_until(|| server.puts.load(Ordering::Relaxed) > 0)?;
    assert!(root.path().join("primary/photo").exists());
    fs::write(&replacement, settings(0, 0))?;
    fs::rename(&replacement, &config_path)?;
    wait_until(|| !root.path().join("primary/photo").exists())?;
    assert!(
        service.0.try_wait()?.is_none(),
        "same daemon must remain running"
    );
    let restored = Store::new(Config::load(&config_path)?)?;
    let mut reader = None;
    wait_until(|| {
        reader = restored.read(Path::new("photo")).ok();
        reader.is_some()
    })?;
    let mut bytes = Vec::new();
    reader.unwrap().read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"hot reload content");
    Ok(())
}

#[test]
fn multiple_webdav_copies_are_verified_before_local_gc() -> Result<()> {
    let first = Webdav::start(false);
    let second = Webdav::start(false);
    let root = tempfile::tempdir()?;
    let store = Store::initialize(config(root.path(), &[&first, &second]))?;
    let path = Path::new("photo.jpg");
    {
        let mut writer = store.write(path)?;
        writer.write_all(b"original photo")?;
        writer.sync_all()?;
    }
    assert!(store.reconcile(path)?);
    assert!(store.reconcile(path)?);
    assert!(!root.path().join("primary").join(path).exists());
    let mut bytes = Vec::new();
    store.read(path)?.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"original photo");
    assert!(store.write(path).is_err(), "archived files are read-only");
    assert!(store.check(path)?);
    Ok(())
}

#[test]
fn failed_remote_verification_preserves_local_content() -> Result<()> {
    let server = Webdav::start(true);
    let root = tempfile::tempdir()?;
    let store = Store::initialize(config(root.path(), &[&server]))?;
    let path = Path::new("important");
    {
        let mut writer = store.write(path)?;
        writer.write_all(b"safe locally")?;
    }
    assert!(store.reconcile(path).is_err());
    assert_eq!(
        std::fs::read(root.path().join("primary").join(path))?,
        b"safe locally"
    );
    let mut bytes = Vec::new();
    store.read(path)?.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"safe locally");
    Ok(())
}

#[test]
fn delay_protects_locally_and_ttl_survives_restart() -> Result<()> {
    let first = Webdav::start(false);
    let second = Webdav::start(false);
    let root = tempfile::tempdir()?;
    let mut settings = config(root.path(), &[&first, &second]);
    settings.backup[0].archive.as_mut().unwrap().after_secs = 3600;
    let store = Store::initialize(settings)?;
    let path = Path::new("photo.jpg");
    store.write(path)?.write_all(b"family photo")?;
    store.reconcile(path)?;
    assert_eq!(
        first.puts.load(Ordering::Relaxed) + second.puts.load(Ordering::Relaxed),
        0
    );
    assert_eq!(
        std::fs::read(root.path().join("replica-1/photo.jpg"))?,
        b"family photo"
    );
    drop(store);
    let mut settings = config(root.path(), &[&first, &second]);
    settings.backup[0].archive.as_mut().unwrap().local_ttl_secs = 2;
    let store = Store::new(settings)?;
    store.reconcile(path)?;
    assert!(store.write(path).is_err());
    assert!(root.path().join("primary/photo.jpg").exists());
    assert!(root.path().join("replica-1/photo.jpg").exists());
    let puts = first.puts.load(Ordering::Relaxed) + second.puts.load(Ordering::Relaxed);
    drop(store);
    thread::sleep(Duration::from_secs(3));
    let mut settings = config(root.path(), &[&first, &second]);
    settings.backup[0].archive.as_mut().unwrap().local_ttl_secs = 2;
    let store = Store::new(settings)?;
    first.corrupt.store(true, Ordering::Relaxed);
    assert!(store.reconcile(path).is_err());
    assert!(root.path().join("primary/photo.jpg").exists());
    assert!(root.path().join("replica-1/photo.jpg").exists());
    first.corrupt.store(false, Ordering::Relaxed);
    store.reconcile(path)?;
    assert!(!root.path().join("primary/photo.jpg").exists());
    assert!(!root.path().join("replica-1/photo.jpg").exists());
    assert_eq!(
        first.puts.load(Ordering::Relaxed) + second.puts.load(Ordering::Relaxed),
        puts
    );
    assert!(store.check(path)?);
    Ok(())
}

#[test]
fn failed_archiving_publishes_recoverable_local_descriptors() -> Result<()> {
    let first = Webdav::start(true);
    let second = Webdav::start(true);
    let root = tempfile::tempdir()?;
    let store = Store::initialize(config(root.path(), &[&first, &second]))?;
    let path = Path::new("safe.jpg");
    store.write(path)?.write_all(b"local protection")?;
    assert!(store.reconcile(path).is_err());
    assert_eq!(
        std::fs::read(root.path().join("replica-1/safe.jpg"))?,
        b"local protection"
    );
    assert!(root.path().join("replica-1/.tier/meta/safe.jpg").is_file());
    assert!(store.check(path)?);
    drop(store);
    let store = Store::new(config(root.path(), &[&first, &second]))?;
    assert!(store.check(path)?);
    Ok(())
}

#[test]
fn automatic_target_selection_skips_failed_verification() -> Result<()> {
    let bad = Webdav::start(true);
    let good = Webdav::start(false);
    let root = tempfile::tempdir()?;
    let mut settings = config(root.path(), &[&bad, &good]);
    settings.backup[0].count = 1;
    settings.backup[0].archive.as_mut().unwrap().count = 1;
    let store = Store::initialize(settings)?;
    let path = Path::new("photo");
    store.write(path)?.write_all(b"photo")?;
    // This test exercises target selection, not wall-clock archive scheduling.
    // Keep the filesystem timestamp away from the current second boundary.
    std::fs::File::open(root.path().join("primary").join(path))?
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))?;
    store.reconcile(path)?;
    store.reconcile(path)?;
    assert_eq!(good.puts.load(Ordering::Relaxed), 1);
    assert!(store.check(path)?);
    Ok(())
}

#[test]
fn changing_archive_target_reprotects_locally_and_restarts_retention() -> Result<()> {
    let first = Webdav::start(false);
    let second = Webdav::start(false);
    let root = tempfile::tempdir()?;
    let mut initial = config(root.path(), &[&first]);
    initial.backup[0].archive.as_mut().unwrap().local_ttl_secs = 3600;
    let store = Store::initialize(initial)?;
    let path = Path::new("memory.jpg");
    store.write(path)?.write_all(b"irreplaceable memory")?;
    store.reconcile(path)?;
    assert_eq!(first.puts.load(Ordering::Relaxed), 1);
    drop(store);

    let migrated_config = || {
        let mut settings = config(root.path(), &[&second]);
        let target = settings.archives.remove("dav0").unwrap();
        settings.archives.insert("replacement".into(), target);
        settings.backup[0].archive.as_mut().unwrap().local_ttl_secs = 3600;
        settings
    };
    let store = Store::new(migrated_config())?;
    store.reconcile(path)?;
    assert_eq!(second.puts.load(Ordering::Relaxed), 1);
    assert!(root.path().join("primary/memory.jpg").is_file());
    assert!(store.check(path)?);
    assert!(store.write(path).is_err());
    drop(store);

    // A restart during the new TTL retains the local copy and does not repeat
    // the upload or reference the removed target during verification.
    let store = Store::new(migrated_config())?;
    store.reconcile(path)?;
    assert!(root.path().join("primary/memory.jpg").is_file());
    assert_eq!(second.puts.load(Ordering::Relaxed), 1);
    assert!(store.check(path)?);
    drop(store);

    let mut expired = migrated_config();
    expired.backup[0].archive.as_mut().unwrap().local_ttl_secs = 0;
    let store = Store::new(expired)?;
    store.reconcile(path)?;
    assert!(!root.path().join("primary/memory.jpg").exists());
    let mut bytes = Vec::new();
    store.read(path)?.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"irreplaceable memory");
    Ok(())
}

#[test]
fn changing_url_under_same_target_name_requires_new_verified_archive() -> Result<()> {
    let first = Webdav::start(false);
    let replacement = Webdav::start(false);
    let root = tempfile::tempdir()?;
    let mut initial = config(root.path(), &[&first]);
    initial.backup[0].archive.as_mut().unwrap().local_ttl_secs = 3600;
    let store = Store::initialize(initial)?;
    let path = Path::new("memory");
    store.write(path)?.write_all(b"unique photo")?;
    store.reconcile(path)?;
    drop(store);
    let mut changed = config(root.path(), &[&replacement]);
    changed.backup[0].archive.as_mut().unwrap().local_ttl_secs = 3600;
    let store = Store::new(changed)?;
    assert!(
        store.check(path).is_err(),
        "old archive must not count at new URL"
    );
    store.reconcile(path)?;
    assert_eq!(replacement.puts.load(Ordering::Relaxed), 1);
    assert!(root.path().join("primary/memory").is_file());
    assert!(store.check(path)?);
    Ok(())
}

// Simulate persisted timestamps from a machine whose wall clock was ahead.
// Recompute both descriptor slot checksums so this exercises retention policy,
// not damaged-descriptor handling. This never touches live user storage.
fn set_archived_at(path: &Path, timestamp: u64) -> Result<()> {
    edit_archived_descriptor(path, |state| state["archived_at"] = timestamp.into())
}

fn edit_archived_descriptor(
    path: &Path,
    mut edit: impl FnMut(&mut serde_json::Value),
) -> Result<()> {
    use sha2::{Digest, Sha256};
    const SLOT: usize = 64 * 1024;
    const HEADER: usize = 44;
    let mut bytes = std::fs::read(path)?;
    for offset in [0, SLOT] {
        if bytes.len() < offset + HEADER {
            continue;
        }
        let length = u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into()?) as usize;
        if length == 0 || bytes.len() < offset + HEADER + length {
            continue;
        }
        let mut state: serde_json::Value =
            serde_json::from_slice(&bytes[offset + HEADER..offset + HEADER + length])?;
        if state["archives"].as_array().is_none_or(Vec::is_empty) {
            continue;
        }
        edit(&mut state);
        let payload = serde_json::to_vec(&state)?;
        assert!(payload.len() < SLOT - HEADER);
        bytes.resize(bytes.len().max(offset + HEADER + payload.len()), 0);
        bytes[offset + 8..offset + 12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let mut hash = Sha256::new();
        hash.update(&bytes[offset..offset + 12]);
        hash.update(&payload);
        bytes[offset + 12..offset + HEADER].copy_from_slice(&hash.finalize());
        bytes[offset + HEADER..offset + HEADER + payload.len()].copy_from_slice(&payload);
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

#[test]
fn zero_ttl_does_not_delete_before_persisted_archive_time() -> Result<()> {
    let server = Webdav::start(false);
    let root = tempfile::tempdir()?;
    let store = Store::initialize(config(root.path(), &[&server]))?;
    let path = Path::new("future.jpg");
    store.write(path)?.write_all(b"clock rollback photo")?;
    store.reconcile(path)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let descriptor = root.path().join("primary/.tier/meta/future.jpg");
    set_archived_at(&descriptor, now + 3600)?;
    drop(store);
    let store = Store::new(config(root.path(), &[&server]))?;
    store.scan_pending()?;
    assert!(root.path().join("primary/future.jpg").is_file());
    store.reconcile(path)?;
    assert!(root.path().join("primary/future.jpg").is_file());
    set_archived_at(&descriptor, now)?;
    store.reconcile(path)?;
    assert!(!root.path().join("primary/future.jpg").exists());
    let mut bytes = Vec::new();
    store.read(path)?.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"clock rollback photo");
    Ok(())
}

#[test]
fn endpoint_migration_preserves_pending_retirement_until_new_archive_verified() -> Result<()> {
    let first = Webdav::start(false);
    let replacement = Webdav::start(true);
    let root = tempfile::tempdir()?;
    let mut initial = config(root.path(), &[&first]);
    initial.backup[0].archive.as_mut().unwrap().local_ttl_secs = 3600;
    let store = Store::initialize(initial)?;
    let path = Path::new("memory.jpg");
    store.write(path)?.write_all(b"retiring copy photo")?;
    store.reconcile(path)?;
    drop(store);
    // Simulate a crash after publishing retirement intent but before removing
    // the old extra local content, then change the endpoint while recovering.
    let retiring = root.path().join("replica-1/memory.jpg");
    std::fs::write(&retiring, b"retiring copy photo")?;
    edit_archived_descriptor(
        &root.path().join("primary/.tier/meta/memory.jpg"),
        |state| {
            state["retiring"] = serde_json::json!(["disk1"]);
        },
    )?;
    let mut changed = config(root.path(), &[&replacement]);
    changed.backup[0].archive.as_mut().unwrap().local_ttl_secs = 3600;
    let store = Store::new(changed)?;
    assert!(store.reconcile(path).is_err());
    assert!(retiring.is_file());
    assert!(root.path().join("primary/memory.jpg").is_file());
    replacement.corrupt.store(false, Ordering::Relaxed);
    store.reconcile(path)?;
    assert!(!retiring.exists());
    assert!(root.path().join("primary/memory.jpg").is_file());
    assert!(store.check(path)?);
    Ok(())
}

#[test]
fn scan_runs_remote_requests_in_parallel_with_global_and_target_caps() -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::sync::{Condvar, Mutex};

    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(0usize), Condvar::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    let mut urls = Vec::new();
    let target_peaks: Vec<_> = (0..2).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    for target_peak in &target_peaks {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        urls.push(format!("http://{}/", server.server_addr()));
        let (active, peak, gate, stop, target_peak) = (
            active.clone(),
            peak.clone(),
            gate.clone(),
            stop.clone(),
            target_peak.clone(),
        );
        workers.push(thread::spawn(move || {
            let objects = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
            let target_active = Arc::new(AtomicUsize::new(0));
            let first = Arc::new(AtomicBool::new(true));
            let mut requests = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap()
                else {
                    continue;
                };
                let (active, peak, gate, target_peak, target_active, objects, first) = (
                    active.clone(),
                    peak.clone(),
                    gate.clone(),
                    target_peak.clone(),
                    target_active.clone(),
                    objects.clone(),
                    first.clone(),
                );
                requests.push(thread::spawn(move || {
                    peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    target_peak.fetch_max(
                        target_active.fetch_add(1, Ordering::SeqCst) + 1,
                        Ordering::SeqCst,
                    );
                    if first.swap(false, Ordering::SeqCst) {
                        let (lock, wake) = &*gate;
                        let mut arrived = lock.lock().unwrap();
                        *arrived += 1;
                        wake.notify_all();
                        let _ = wake
                            .wait_timeout_while(arrived, Duration::from_secs(2), |arrived| {
                                *arrived < 2
                            })
                            .unwrap();
                    }
                    let path = request.url().to_owned();
                    let response = if request.method().as_str() == "PUT" {
                        let mut body = Vec::new();
                        request.as_reader().read_to_end(&mut body).unwrap();
                        objects.lock().unwrap().insert(path, body);
                        tiny_http::Response::from_data(Vec::new()).with_status_code(201)
                    } else {
                        match objects.lock().unwrap().get(&path) {
                            Some(body) => tiny_http::Response::from_data(body.clone()),
                            None => {
                                tiny_http::Response::from_data(Vec::new()).with_status_code(404)
                            }
                        }
                    };
                    // End accounting before emitting the final response; the
                    // client may immediately reuse its slot after receiving it.
                    target_active.fetch_sub(1, Ordering::SeqCst);
                    active.fetch_sub(1, Ordering::SeqCst);
                    request.respond(response).unwrap();
                }));
            }
            for request in requests {
                request.join().unwrap();
            }
        }));
    }
    let root = tempfile::tempdir()?;
    let primary = root.path().join("primary");
    let replica = root.path().join("replica");
    std::fs::create_dir(&primary)?;
    std::fs::create_dir(&replica)?;
    let mut settings: Config = toml::from_str(&format!(
        "primary='{}'\n[replicas]\nmetadata='{}'\n[background]\nworkers=4\narchive_concurrency=2\n[archives.dav0]\nurl='{}'\nconcurrency=1\n[archives.dav1]\nurl='{}'\nconcurrency=1\n[[backup]]\npath=''\ncount=1\ngc=true\n[backup.archive]\ncount=1\n",
        primary.display(),
        replica.display(),
        urls[0],
        urls[1]
    ))?;
    settings.validate()?;
    settings.background.workers = 4;
    let result = (|| -> Result<()> {
        let store = Store::initialize(settings)?;
        // Choose content whose stable target ordering selects different first
        // servers, so the first request barrier proves real worker overlap.
        let mut selected = [0, 0];
        for index in 0..1000 {
            let content = format!("concurrency photo {index}");
            let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
            let keys: Vec<_> = (0..2)
                .map(|target| {
                    format!(
                        "{:x}",
                        Sha256::digest(format!("{digest}:dav{target}").as_bytes())
                    )
                })
                .collect();
            let target = usize::from(keys[1] < keys[0]);
            if selected[target] < 2 {
                store
                    .write(Path::new(&format!("photo-{}-{target}", selected[target])))?
                    .write_all(content.as_bytes())?;
                selected[target] += 1;
            }
            if selected.iter().all(|value| *value == 2) {
                break;
            }
        }
        assert!(selected.iter().all(|value| *value == 2));
        let report = store.scan_pending()?;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.completed, 4);
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert!(
            target_peaks
                .iter()
                .all(|peak| peak.load(Ordering::SeqCst) == 1)
        );
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    for worker in workers {
        worker.join().unwrap();
    }
    result
}

#[test]
fn identical_files_reuse_verified_archive_without_overwriting() -> Result<()> {
    let server = Webdav::start(false);
    let root = tempfile::tempdir()?;
    let store = Store::initialize(config(root.path(), &[&server]))?;
    for name in ["first.jpg", "second.jpg"] {
        store
            .write(Path::new(name))?
            .write_all(b"same precious photograph")?;
    }
    let report = store.scan_pending()?;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(server.puts.load(Ordering::Relaxed), 1);
    let report = store.scan_pending()?;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    for name in ["first.jpg", "second.jpg"] {
        assert!(!root.path().join("primary").join(name).exists());
        let mut bytes = Vec::new();
        store.read(Path::new(name))?.read_to_end(&mut bytes)?;
        assert_eq!(bytes, b"same precious photograph");
    }
    assert_eq!(server.puts.load(Ordering::Relaxed), 1);
    Ok(())
}
