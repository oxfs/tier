use anyhow::Result;
use std::{
    collections::HashMap,
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tempfile::tempdir;
use tier::{archive::ArchivePolicy, core::Engine};
use tiny_http::{Method, Response, Server};

#[test]
fn evict_only_after_verified_upload_and_recall_by_digest() -> Result<()> {
    let http = Server::http("127.0.0.1:0").map_err(|e| anyhow::anyhow!("{e}"))?;
    let url = format!("http://{}/dav", http.server_addr());
    let corrupt = Arc::new(AtomicBool::new(true));
    let bad = corrupt.clone();
    let unavailable = Arc::new(std::sync::Mutex::new(String::new()));
    let missing = unavailable.clone();
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let worker = thread::spawn(move || -> Result<()> {
        let mut objects = HashMap::new();
        while !stop.load(Ordering::Relaxed) {
            let Some(mut req) = http.recv_timeout(Duration::from_millis(50))? else {
                continue;
            };
            let key = req.url().to_owned();
            let (status, body) = match req.method() {
                Method::Put => {
                    let mut buf = Vec::new();
                    req.as_reader().read_to_end(&mut buf)?;
                    objects.insert(key, buf);
                    (201, vec![])
                }
                Method::Get => match objects.get(&key) {
                    _ if *missing.lock().unwrap() == key => (404, vec![]),
                    None => (404, vec![]),
                    Some(_) if bad.load(Ordering::Relaxed) => (200, b"corrupt".to_vec()),
                    Some(data) => (200, data.clone()),
                },
                Method::Delete => {
                    objects.remove(&key);
                    (204, vec![])
                }
                _ => (405, vec![]),
            };
            req.respond(Response::from_data(body).with_status_code(status))?;
        }
        Ok(())
    });
    let work = tempdir()?;
    let disk = work.path().join("disk");
    let replica = work.path().join("replica");
    fs::create_dir(&disk)?;
    fs::create_dir(&replica)?;
    let engine = Engine::open(&work.path().join("db"))?;
    engine.add_target("disk", &disk)?;
    engine.register_target("replica", &replica, None, true)?;
    engine.set_policy(
        "/",
        &tier::core::Policy {
            write_cache: vec!["disk".into()],
            persistent_storage: vec!["replica".into()],
            persistent_copies: 1,
            ..Default::default()
        },
    )?;
    engine.add_archive("remote", &url, None, None)?;
    engine.set_archive_policy(
        "/",
        &ArchivePolicy {
            targets: vec!["remote".into()],
            copies: 1,
            delay_secs: 0,
            retain_local: 0,
        },
    )?;
    engine.put("/photo", b"photo bytes")?;
    engine.reconcile()?;
    assert!(engine.archive_tick().is_err());
    assert_eq!(engine.get("/photo")?, b"photo bytes");
    corrupt.store(false, Ordering::Relaxed);
    engine.archive_tick()?;
    assert_eq!(engine.state()?["health"][0]["status"], "remote_only");
    // With no local source, corrupt remote content is never accepted on recall.
    corrupt.store(true, Ordering::Relaxed);
    assert!(engine.get("/photo").is_err());
    corrupt.store(false, Ordering::Relaxed);
    assert_eq!(engine.get("/photo")?, b"photo bytes");
    // Disabling archive retains it until persistent placement is repaired.
    engine.set_archive_policy(
        "/",
        &ArchivePolicy {
            targets: vec![],
            copies: 0,
            delay_secs: 0,
            retain_local: 1,
        },
    )?;
    engine.reconcile()?;
    engine.archive_tick()?;
    assert_eq!(engine.state()?["archive_targets"][0]["object_count"], 0);
    assert_eq!(engine.get("/photo")?, b"photo bytes");
    // A damaged preferred local target cannot displace a healthy cache copy.
    let id = engine.stat("/photo")?.id;
    let object = fs::read_dir(replica.join("objects").join(id))?
        .next()
        .unwrap()?
        .path();
    fs::write(object, b"wrong bytes")?;
    engine.set_archive_policy(
        "/",
        &ArchivePolicy {
            targets: vec!["remote".into()],
            copies: 1,
            delay_secs: 0,
            retain_local: 1,
        },
    )?;
    engine.archive_tick()?;
    assert_eq!(engine.get("/photo")?, b"photo bytes");
    corrupt.store(true, Ordering::Relaxed);
    assert!(engine.archive_tick().is_err());
    assert_eq!(engine.get("/photo")?, b"photo bytes");
    done.store(true, Ordering::Relaxed);
    worker.join().unwrap()?;
    Ok(())
}
