use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tier::config::Config;

pub type Health = Arc<Mutex<Value>>;

pub fn record(health: &Health, event: &str, details: &Value) {
    let entry = json!({"event":event,"details":details,"time":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()});
    let mut health = health.lock().unwrap();
    health["last"] = entry.clone();
    health[event] = entry;
}

fn revision(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

fn configuration(path: &Path) -> Result<Value> {
    let content = fs::read_to_string(path)?;
    Ok(json!({"revision":revision(&content),"content":content}))
}

fn save(path: &Path, body: &Value, validate_only: bool) -> Result<Value> {
    let content = body["content"].as_str().context("缺少配置内容")?;
    let config: Config = toml::from_str(content)?;
    config.validate()?;
    if !validate_only {
        ensure!(
            body["revision"].as_str() == Some(&revision(&fs::read_to_string(path)?)),
            "配置已被外部修改，请保留草稿并重新加载"
        );
        let parent = path.parent().context("配置路径没有父目录")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary
            .as_file()
            .set_permissions(fs::metadata(path)?.permissions())?;
        temporary.write_all(content.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(json!({"revision":revision(content)}))
}

fn health(path: &Path, state: &Health) -> Value {
    let history = state.lock().unwrap().clone();
    let last = history.get("last").cloned();
    let config = match Config::load(path) {
        Ok(config) => config,
        Err(error) => {
            return json!({"healthy":false,"last":last,"disks":[],"error":format!("{error:#}")});
        }
    };
    let identities: std::collections::BTreeMap<std::path::PathBuf, String> =
        fs::read(config.primary.join(".tier/storage.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
    let hardware: Value = fs::read("/run/tier-hardware/status.json")
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    let disks: Vec<_> = std::iter::once(("主盘", &config.primary))
        .chain(
            config
                .replicas
                .iter()
                .map(|(name, path)| (name.as_str(), path)),
        )
        .map(|(name, root)| {
            let result = (|| -> Result<(u64, u64)> {
                let canonical = fs::canonicalize(root)?;
                let expected = identities.get(&canonical).context("磁盘未注册")?;
                ensure!(
                    fs::read_to_string(root.join(".tier/disk-id"))? == *expected,
                    "磁盘身份不匹配"
                );
                Ok((fs2::total_space(root)?, fs2::available_space(root)?))
            })();
            match result {
                Ok((total, available)) => {
                    json!({"name":name,"path":root,"total":total,"available":available,"hardware":disk_hardware(root, &hardware)})
                }
                Err(error) => json!({"name":name,"path":root,"error":format!("{error:#}")}),
            }
        })
        .collect();
    let healthy = disks.iter().all(|disk| disk.get("error").is_none()) && reports_healthy(&history);
    json!({"healthy":healthy,"disks":disks,"archives":config.archives.keys().collect::<Vec<_>>(),"policies":config.backup.len(),"last":last,"history":history})
}

fn disk_hardware(root: &Path, snapshot: &Value) -> Value {
    use std::os::unix::fs::MetadataExt;
    let Some(updated) = snapshot["updated_at"].as_u64() else {
        return json!({"error":"尚未采集硬件信息，请检查 tier-hardware.timer"});
    };
    let devices = fs::metadata(root)
        .ok()
        .and_then(|metadata| {
            let device = metadata.dev();
            let key = format!("{}:{}", libc::major(device), libc::minor(device));
            snapshot["devices"][key].as_array().cloned()
        })
        .unwrap_or_default();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    json!({"updated_at":updated,"stale":now.saturating_sub(updated) > 600,
        "devices":devices,"error":if devices.is_empty() { Some("无法映射物理硬盘，网络或虚拟文件系统可能不支持") } else { None }})
}

fn reports_healthy(history: &Value) -> bool {
    history
        .get("last")
        .is_some_and(|last| last["event"] != "error")
        && ["replicate", "audit", "archive_audit"].iter().all(|event| {
            history.get(*event).is_none_or(|report| {
                report["details"]["busy"].as_u64() == Some(0)
                    && report["details"]["errors"]
                        .as_array()
                        .is_some_and(Vec::is_empty)
            })
        })
}

#[derive(Clone)]
struct WebState {
    path: std::path::PathBuf,
    health: Health,
    workers: Arc<tokio::sync::Semaphore>,
    mutation: Arc<Mutex<()>>,
    address: String,
}

async fn api(
    axum::extract::State(state): axum::extract::State<WebState>,
    request: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let result = async {
        let host = request
            .headers()
            .get("host")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let socket: std::net::SocketAddr = state.address.parse()?;
        ensure!(
            socket.ip().is_unspecified()
                || host == state.address
                || host == format!("localhost:{}", socket.port()),
            "invalid Host"
        );
        ensure!(
            request
                .headers()
                .get("X-Tier-Request")
                .is_some_and(|value| value == "1"),
            "missing request header"
        );
        let method = request.method().clone();
        let route = request.uri().path().to_owned();
        let bytes = tokio::time::timeout(
            Duration::from_secs(10),
            axum::body::to_bytes(request.into_body(), 262144),
        )
        .await
        .context("request body timeout")??;
        let permit = state
            .workers
            .clone()
            .try_acquire_owned()
            .context("管理任务繁忙，请稍后重试")?;
        tokio::task::spawn_blocking(move || -> Result<Value> {
            let _permit = permit;
            handle_request(&state, &method, &route, &bytes)
        })
        .await?
    }
    .await;
    let mut response = match result {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error":format!("{error:#}")})),
        )
            .into_response(),
    };
    response.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn handle_request(
    state: &WebState,
    method: &axum::http::Method,
    route: &str,
    bytes: &[u8],
) -> Result<Value> {
    match (method.as_str(), route) {
        ("GET", "/api/health") => Ok(health(&state.path, &state.health)),
        ("GET", "/api/config") => configuration(&state.path),
        ("POST", "/api/init" | "/api/scan") => {
            let _lock = state
                .mutation
                .try_lock()
                .map_err(|_| anyhow::anyhow!("另一个管理操作正在执行，请稍后重试"))?;
            let config = Config::load(&state.path)?;
            if route == "/api/init" {
                tier::Store::initialize(config)?;
                Ok(json!({"ok":true}))
            } else {
                let report = serde_json::to_value(tier::Store::new(config)?.scan()?)?;
                record(&state.health, "audit", &report);
                record(&state.health, "archive_audit", &report);
                Ok(report)
            }
        }
        ("POST", "/api/config" | "/api/validate") => {
            let _lock = state
                .mutation
                .try_lock()
                .map_err(|_| anyhow::anyhow!("另一个管理操作正在执行，请稍后重试"))?;
            save(
                &state.path,
                &serde_json::from_slice(bytes)?,
                route == "/api/validate",
            )
        }
        ("POST", "/api/recover-primary" | "/api/replace-replica") => {
            let body: Value = serde_json::from_slice(bytes)?;
            ensure!(
                body["confirmed"] == true,
                "请确认已停止 SMB 访问并挂载空白替换磁盘"
            );
            let _lock = state
                .mutation
                .try_lock()
                .map_err(|_| anyhow::anyhow!("另一个管理操作正在执行，请稍后重试"))?;
            let config = Config::load(&state.path)?;
            if route == "/api/recover-primary" {
                Ok(json!({"recovered":tier::recovery::recover_primary(config)?}))
            } else {
                tier::recovery::replace_replica(
                    config,
                    body["name"].as_str().context("missing replica name")?,
                )?;
                Ok(json!({"ok":true}))
            }
        }
        _ => anyhow::bail!("unknown API operation"),
    }
}

pub struct Service {
    health: Health,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::ops::Deref for Service {
    type Target = Health;
    fn deref(&self) -> &Health {
        &self.health
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("Web server thread panicked during shutdown");
        }
    }
}

pub fn start(path: &Path, stop: Arc<AtomicBool>) -> Result<Service> {
    let health: Health = Arc::new(Mutex::new(json!({})));
    let Ok(address) = std::env::var("TIER_WEB_LISTEN") else {
        return Ok(Service {
            health,
            stop,
            thread: None,
        });
    };
    let socket: std::net::SocketAddr = address.parse().context("invalid TIER_WEB_LISTEN")?;
    let listener = std::net::TcpListener::bind(socket)?;
    listener.set_nonblocking(true)?;
    let state = WebState {
        path: std::path::absolute(path)?,
        health: health.clone(),
        workers: Arc::new(tokio::sync::Semaphore::new(4)),
        mutation: Arc::new(Mutex::new(())),
        address,
    };
    let assets = std::env::var_os("TIER_WEB_ASSETS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/usr/local/share/tier/web".into());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()?;
    let shutdown_stop = stop.clone();
    let thread = std::thread::spawn(move || {
        runtime.block_on(async move {
            let app = axum::Router::new()
                .route("/api/{*path}", axum::routing::any(api))
                .fallback_service(tower_http::services::ServeDir::new(assets))
                .with_state(state);
            let listener = tokio::net::TcpListener::from_std(listener).expect("valid web listener");
            let shutdown = async move {
                while !shutdown_stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            };
            if let Err(error) = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
            {
                eprintln!("Web server: {error}");
            }
        });
        // Dropping the runtime waits for accepted spawn_blocking mutations,
        // even if an HTTP client disconnected before their completion.
        drop(runtime);
    });
    Ok(Service {
        health,
        stop,
        thread: Some(thread),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hardware_mapping_and_expiry_are_explicit() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir()?;
        let dev = fs::metadata(directory.path())?.dev();
        let key = format!("{}:{}", libc::major(dev), libc::minor(dev));
        let snapshot = json!({"updated_at":1,"devices":{key:[{"model":"disk","temperature":0,"passed":false}]}});
        let result = disk_hardware(directory.path(), &snapshot);
        assert_eq!(result["devices"][0]["temperature"], 0);
        assert_eq!(result["devices"][0]["passed"], false);
        assert_eq!(result["stale"], true);
        assert!(disk_hardware(directory.path(), &Value::Null)["error"].is_string());
        assert!(disk_hardware(directory.path(), &json!({"updated_at":1}))["error"].is_string());
        Ok(())
    }

    #[test]
    fn busy_mutation_rejects_immediately_without_blocking_health() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let state = WebState {
            path: directory.path().join("missing.toml"),
            health: Arc::new(Mutex::new(json!({}))),
            workers: Arc::new(tokio::sync::Semaphore::new(4)),
            mutation: Arc::new(Mutex::new(())),
            address: "127.0.0.1:8739".into(),
        };
        let mutation = state.mutation.clone();
        let _held = mutation.lock().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            for (method, path, expected) in
                [("POST", "/api/scan", 400), ("GET", "/api/health", 200)]
            {
                let request = axum::http::Request::builder()
                    .method(method)
                    .uri(path)
                    .header("Host", "127.0.0.1:8739")
                    .header("X-Tier-Request", "1")
                    .body(axum::body::Body::empty())
                    .unwrap();
                let response = tokio::time::timeout(
                    Duration::from_secs(2),
                    api(axum::extract::State(state.clone()), request),
                )
                .await
                .expect("request blocked on mutation");
                assert_eq!(response.status().as_u16(), expected);
            }
        });
        Ok(())
    }

    #[test]
    fn audit_success_does_not_hide_replication_failure_or_busy_files() {
        let health: Health = Arc::new(Mutex::new(json!({})));
        record(
            &health,
            "replicate",
            &json!({"busy":0,"errors":["upload failed"]}),
        );
        record(&health, "audit", &json!({"busy":0,"errors":[]}));
        assert!(!reports_healthy(&health.lock().unwrap()));
        record(&health, "replicate", &json!({"busy":1,"errors":[]}));
        assert!(!reports_healthy(&health.lock().unwrap()));
        record(&health, "replicate", &json!({"busy":0,"errors":[]}));
        assert!(reports_healthy(&health.lock().unwrap()));
    }
    #[test]
    fn maintenance_history_retains_audit_failures() {
        let health: Health = Arc::new(Mutex::new(json!({})));
        record(&health, "audit", &json!({"errors":["damaged replica"]}));
        record(&health, "replicate", &json!({"errors":[]}));
        let history = health.lock().unwrap();
        assert_eq!(history["audit"]["details"]["errors"][0], "damaged replica");
        assert_eq!(history["last"]["event"], "replicate");
    }

    #[test]
    fn configuration_validation_and_conflicts_preserve_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        let content = "primary='/primary'\n[replicas]\n[[backup]]\npath=''\ncount=1\n";
        fs::write(&path, content)?;
        let original = configuration(&path)?;
        assert!(
            save(
                &path,
                &json!({"content":"bad", "revision":original["revision"]}),
                false
            )
            .is_err()
        );
        assert!(save(&path, &json!({"content":content,"revision":"stale"}), false).is_err());
        assert_eq!(fs::read_to_string(&path)?, content);
        let updated = format!("# preserved comment\n{content}");
        save(
            &path,
            &json!({"content":updated,"revision":original["revision"]}),
            false,
        )?;
        assert_eq!(fs::read_to_string(&path)?, updated);
        Ok(())
    }
}
