use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use tempfile::tempdir;
use tier::{archive::ArchivePolicy, core::Engine};
use tiny_http::{Method, Response, Server};

#[test]
fn discarded_upload_cleanup_does_not_block_namespace_writes() -> Result<()> {
    let http = Server::http("127.0.0.1:0").map_err(|e| anyhow::anyhow!("{e}"))?;
    let work = tempdir()?;
    let disk = work.path().join("disk");
    fs::create_dir(&disk)?;
    let engine = Engine::open(&work.path().join("db"))?;
    engine.add_target("disk", &disk)?;
    engine.set_policy(
        "/",
        &tier::core::Policy {
            write_cache: vec!["disk".into()],
            low_water: 99,
            high_water: 100,
            ..Default::default()
        },
    )?;
    engine.add_archive(
        "remote",
        &format!("http://{}/dav", http.server_addr()),
        None,
        None,
    )?;
    engine.set_archive_policy(
        "/",
        &ArchivePolicy {
            targets: vec!["remote".into()],
            copies: 1,
            delay_secs: 0,
            retain_local: 1,
        },
    )?;
    let contents = b"photo bytes";
    engine.put("/photo", contents)?;

    thread::scope(|scope| -> Result<()> {
        let archive = scope.spawn(|| engine.archive_tick());
        let receive = || {
            http.recv_timeout(Duration::from_secs(5))?
                .context("expected WebDAV request")
        };
        let mut upload = receive()?;
        assert_eq!(upload.method(), &Method::Put);
        let object = upload.url().to_owned();
        let mut body = Vec::new();
        upload.as_reader().read_to_end(&mut body)?;
        assert_eq!(body, contents);
        // Change the name before upload completes so publication must discard it.
        engine.rename("/photo", "/renamed")?;
        upload.respond(Response::empty(201))?;
        let verification = receive()?;
        assert_eq!(verification.method(), &Method::Get);
        assert_eq!(verification.url(), object);
        verification.respond(Response::from_data(body))?;
        let cleanup = receive()?;
        assert_eq!(cleanup.method(), &Method::Delete);
        assert_eq!(cleanup.url(), object);

        let (send, receive) = mpsc::channel();
        let shared = &engine;
        let mkdir = scope.spawn(move || {
            send.send((|| -> Result<()> {
                shared.mkdir("/unrelated")?;
                // A stale upload's slow DELETE must not retain an exclusive pin.
                shared.open_file("/renamed", false, false, false)?.close()
            })())
            .unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(2));
        // Always release the remote request before asserting, including on failure.
        cleanup.respond(Response::empty(204))?;
        archive.join().unwrap()?;
        mkdir.join().unwrap();
        result.context("namespace write blocked behind remote cleanup")??;
        Ok(())
    })?;
    assert!(engine.stat("/unrelated")?.is_dir);
    assert_eq!(engine.get("/renamed")?, contents);
    assert_eq!(engine.state()?["archive_targets"][0]["object_count"], 0);
    Ok(())
}

#[test]
fn evict_only_after_verified_upload_and_recall_by_digest() -> Result<()> {
    let http = Server::http("127.0.0.1:0").map_err(|e| anyhow::anyhow!("{e}"))?;
    let url = format!("http://{}/dav", http.server_addr());
    let downloads = Arc::new(AtomicUsize::new(0));
    let download_count = downloads.clone();
    let corrupt = Arc::new(AtomicBool::new(true));
    let bad = corrupt.clone();
    let reject_uploads = Arc::new(AtomicBool::new(false));
    let reject = reject_uploads.clone();
    let objects = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let stored = objects.clone();
    let unavailable = Arc::new(std::sync::Mutex::new(String::new()));
    let missing = unavailable.clone();
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let worker = thread::spawn(move || -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            let Some(mut req) = http.recv_timeout(Duration::from_millis(50))? else {
                continue;
            };
            let key = req.url().to_owned();
            let mut objects = stored.lock().unwrap();
            let (status, body) = match req.method() {
                Method::Put if reject.load(Ordering::Relaxed) => (503, vec![]),
                Method::Put => {
                    let mut buf = Vec::new();
                    req.as_reader().read_to_end(&mut buf)?;
                    objects.insert(key, buf);
                    (201, vec![])
                }
                Method::Get => {
                    download_count.fetch_add(1, Ordering::Relaxed);
                    match objects.get(&key) {
                        _ if *missing.lock().unwrap() == key => (404, vec![]),
                        None => (404, vec![]),
                        Some(_) if bad.load(Ordering::Relaxed) => (200, b"corrupt".to_vec()),
                        Some(data) => (200, data.clone()),
                    }
                }
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
            low_water: 99,
            high_water: 100,
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
    // Browsing/opening archive-only files must not download them. Concurrent
    // first reads share the FileState and publish only one verified local copy.
    let before = downloads.load(Ordering::Relaxed);
    assert_eq!(engine.stat("/photo")?.size, 11);
    assert_eq!(engine.list("/")?, ["photo"]);
    let first = engine.open_file("/photo", false, false, false)?;
    let second = engine.open_file("/photo", false, false, false)?;
    assert_eq!(first.read_at(0, &mut [])?, 0);
    assert_eq!(downloads.load(Ordering::Relaxed), before);
    let barrier = std::sync::Barrier::new(2);
    let read = |file: tier::core::FileHandle| -> Result<()> {
        barrier.wait();
        let mut bytes = [0; 11];
        assert_eq!(file.read_at(0, &mut bytes)?, bytes.len());
        assert_eq!(&bytes, b"photo bytes");
        file.close()
    };
    thread::scope(|scope| -> Result<()> {
        let other = scope.spawn(|| read(second));
        read(first)?;
        other.join().unwrap()?;
        Ok(())
    })?;
    assert_eq!(downloads.load(Ordering::Relaxed), before + 1);
    engine.archive_tick()?;
    assert_eq!(engine.state()?["health"][0]["status"], "remote_only");
    // Failed background renaming keeps the previous readable remote mapping.
    let old_objects = objects.lock().unwrap().clone();
    engine.rename("/photo", "/旅行 #?%.jpg")?;
    reject_uploads.store(true, Ordering::Relaxed);
    assert!(engine.archive_tick().is_err());
    assert_eq!(*objects.lock().unwrap(), old_objects);
    reject_uploads.store(false, Ordering::Relaxed);
    engine.archive_tick()?;
    {
        let stored = objects.lock().unwrap();
        assert_eq!(stored.len(), 1);
        assert!(
            stored
                .keys()
                .next()
                .unwrap()
                .ends_with("%E6%97%85%E8%A1%8C%20%23%3F%25.jpg")
        );
        assert!(old_objects.keys().all(|key| !stored.contains_key(key)));
    }
    assert_eq!(engine.get("/旅行 #?%.jpg")?, b"photo bytes");
    engine.rename("/旅行 #?%.jpg", "/photo")?;
    engine.archive_tick()?;
    // With no local source, corrupt remote content is never accepted on recall.
    corrupt.store(true, Ordering::Relaxed);
    let archived = engine.open_file("/photo", false, false, false)?;
    assert_eq!(engine.stat("/photo")?.size, 11);
    assert_eq!(archived.read_at(0, &mut [])?, 0);
    archived.sync()?;
    assert_eq!(engine.state()?["health"][0]["status"], "remote_only");
    assert!(archived.read_at(0, &mut [0; 11]).is_err());
    archived.close()?;
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
    // This test needs two live copies regardless of the runner's disk usage.
    assert_eq!(
        engine.state()?["health"][0]["readable_local"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
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
