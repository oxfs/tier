use anyhow::Result;
use serde_json::json;
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
use tier::{cloud::CloudPolicy, core::Engine};
use tiny_http::{Method, Response, Server};

#[test]
fn evict_only_after_verified_upload_and_recall_by_digest() -> Result<()> {
    let http = Server::http("127.0.0.1:0").map_err(|e| anyhow::anyhow!("{e}"))?;
    let url = format!("http://{}/dav", http.server_addr());
    let corrupt = Arc::new(AtomicBool::new(true));
    let bad = corrupt.clone();
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
    fs::create_dir(&disk)?;
    let db = work.path().join("tier.db");
    let engine = Engine::open(&db)?;
    engine.add_target("disk", &disk)?;
    engine.set_policy(
        "/",
        &serde_json::from_value(
            json!({"front":["disk"],"front_count":1,"long":["disk"],"long_count":1,"delay_secs":0}),
        )?,
    )?;
    engine.add_archive("remote", &url, None, None)?;
    engine.set_cloud_policy(
        "/",
        &CloudPolicy {
            targets: vec!["remote".into()],
            copies: 1,
            delay_secs: 0,
            retain_local: 0,
        },
    )?;
    engine.put("/photo", b"photo bytes")?;
    engine.reconcile()?;
    assert!(engine.archive_tick().is_err());
    assert_eq!(engine.snapshot()?["copies"].as_array().unwrap().len(), 1);
    assert_eq!(engine.get("/photo")?, b"photo bytes");
    corrupt.store(false, Ordering::Relaxed);
    engine.archive_tick()?;
    assert!(engine.snapshot()?["copies"].as_array().unwrap().is_empty());
    drop(engine);
    let reopened = Engine::open(&db)?;
    assert_eq!(reopened.get("/photo")?, b"photo bytes");
    corrupt.store(true, Ordering::Relaxed);
    assert!(reopened.get("/photo").is_err());
    reopened.set_cloud_policy(
        "/",
        &CloudPolicy {
            targets: vec![],
            copies: 0,
            delay_secs: 0,
            retain_local: 1,
        },
    )?;
    reopened.archive_tick()?;
    assert_eq!(
        reopened.snapshot()?["cloud_copies"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    corrupt.store(false, Ordering::Relaxed);
    reopened.reconcile()?;
    reopened.archive_tick()?;
    assert!(
        reopened.snapshot()?["cloud_copies"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(reopened.get("/photo")?, b"photo bytes");
    done.store(true, Ordering::Relaxed);
    worker.join().unwrap()?;
    Ok(())
}
