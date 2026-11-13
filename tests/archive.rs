use anyhow::Result;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
}
impl Webdav {
    fn start(corrupt: bool) -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", server.server_addr());
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
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
                        let mut body = Vec::new();
                        request.as_reader().read_to_end(&mut body).unwrap();
                        objects.insert(path, body);
                        request.respond(tiny_http::Response::empty(201)).unwrap();
                    }
                    "GET" => {
                        if let Some(body) = objects.get(&path) {
                            let bytes = if corrupt {
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
    Config {
        primary: root.to_owned(),
        replicas: BTreeMap::new(),
        background: Background::default(),
        archives: servers
            .iter()
            .enumerate()
            .map(|(i, s)| {
                (
                    format!("dav{i}"),
                    ArchiveTarget {
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
                targets: (0..servers.len()).map(|i| format!("dav{i}")).collect(),
                count: servers.len(),
            }),
        }],
    }
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
    assert!(!root.path().join(path).exists());
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
    assert_eq!(std::fs::read(root.path().join(path))?, b"safe locally");
    let mut bytes = Vec::new();
    store.read(path)?.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"safe locally");
    Ok(())
}
