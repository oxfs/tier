mod command;

use crate::{core::Engine, service::Maintenance};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, Request, State, rejection::JsonRejection},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use command::Command;
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc};

#[derive(Clone)]
struct Backend {
    engine: Engine,
    maintenance: Arc<Maintenance>,
    // Bound accepted synchronous admin work before entering the blocking pool.
    limiter: Arc<tokio::sync::Semaphore>,
}

pub(crate) struct Server {
    pub(crate) addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    request_limit: u32,
    limiter: Arc<tokio::sync::Semaphore>,
}

impl Server {
    pub(crate) async fn shutdown(mut self) -> Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        // An idle or partial HTTP request must not hold process shutdown open.
        let result =
            match tokio::time::timeout(std::time::Duration::from_secs(5), &mut self.task).await {
                Ok(result) => result
                    .context("administration server task failed")
                    .and_then(|result| result.context("administration server failed")),
                Err(_) => {
                    self.task.abort();
                    let _ = (&mut self.task).await;
                    tracing::warn!("administration shutdown timed out");
                    Ok(())
                }
            };
        // Aborting an HTTP handler cannot cancel already running spawn_blocking
        // work. Drain its permits before the service syncs and closes metadata.
        let _drained = self.limiter.acquire_many(self.request_limit).await?;
        self.limiter.close();
        result
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

pub(crate) async fn start(
    addr: SocketAddr,
    engine: Engine,
    maintenance: Arc<Maintenance>,
    request_limit: u32,
) -> Result<Server> {
    ensure!(addr.ip().is_loopback(), "admin listener must use loopback");
    ensure!(
        request_limit > 0 && request_limit as usize <= tokio::sync::Semaphore::MAX_PERMITS,
        "admin request limit is out of range"
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    let limiter = Arc::new(tokio::sync::Semaphore::new(request_limit as usize));
    let app = router(
        addr,
        Backend {
            engine,
            maintenance,
            limiter: limiter.clone(),
        },
    );
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown.await;
            })
            .await;
        if let Err(error) = &result {
            tracing::error!(%error, "administration server stopped");
        }
        result
    });
    Ok(Server {
        addr,
        stop: Some(stop),
        task,
        request_limit,
        limiter,
    })
}

fn router(addr: SocketAddr, backend: Backend) -> Router {
    Router::new()
        .route(
            "/",
            get(async || Html(include_str!("../web/dist/index.html"))),
        )
        .route(
            "/assets/app.js",
            get(async || {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/dist/assets/app.js"),
                )
            }),
        )
        .route(
            "/assets/app.css",
            get(async || {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/dist/assets/app.css"),
                )
            }),
        )
        .route("/api/state", get(read_state))
        .route("/api/trash", get(read_trash))
        .route("/api/action", post(execute))
        .layer(DefaultBodyLimit::max(65536))
        .layer(middleware::from_fn_with_state::<
            _,
            _,
            (State<SocketAddr>, Request),
        >(addr, check_origin))
        .with_state(backend)
}

fn allowed_origin(addr: SocketAddr, request: &Request) -> bool {
    let host = addr.to_string();
    let matches = |name, expected: &str| {
        request.headers().get(name).and_then(|v| v.to_str().ok()) == Some(expected)
    };
    matches(header::HOST, &host)
        && (request.method() != axum::http::Method::POST
            || matches(header::ORIGIN, &format!("http://{host}")))
}

async fn check_origin(State(addr): State<SocketAddr>, request: Request, next: Next) -> Response {
    if !allowed_origin(addr, &request) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    headers.insert(header::CONTENT_SECURITY_POLICY,"default-src 'self'; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'; object-src 'none'; base-uri 'none'".parse().unwrap());
    response
}

async fn blocking(
    backend: Backend,
    changes_files: bool,
    gate: Option<Arc<tokio::sync::Mutex<()>>>,
    operation: impl FnOnce(&Backend) -> Result<Value> + Send + 'static,
) -> Response {
    let Ok(permit) = backend.limiter.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"administration busy"})),
        )
            .into_response();
    };
    let maintenance = match gate {
        Some(gate) => Some(gate.lock_owned().await),
        None => None,
    };
    let mutation = if changes_files {
        match backend.engine.direct_access().mutation().await {
            Ok(permit) => Some(permit),
            Err(error) => return bad_request(error.to_string()),
        }
    } else {
        None
    };
    match tokio::task::spawn_blocking(move || {
        let (_permit, _maintenance, _mutation) = (permit, maintenance, mutation);
        operation(&backend)
    })
    .await
    .context("admin worker failed")
    .and_then(|r| r)
    {
        Ok(value) => Json(value).into_response(),
        Err(error) => bad_request(error.to_string()),
    }
}
fn bad_request(error: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response()
}

#[derive(serde::Deserialize)]
struct TrashQuery {
    page: Option<u64>,
    page_size: Option<u64>,
}

async fn read_trash(State(backend): State<Backend>, Query(query): Query<TrashQuery>) -> Response {
    blocking(backend, false, None, move |backend| {
        backend
            .engine
            .trash_page(query.page.unwrap_or(1), query.page_size.unwrap_or(25))
    })
    .await
}

async fn read_state(State(backend): State<Backend>) -> Response {
    blocking(backend, false, None, |backend| backend.engine.state()).await
}
async fn execute(
    State(backend): State<Backend>,
    body: Result<Json<Command>, JsonRejection>,
) -> Response {
    match body {
        Ok(Json(command)) => {
            let gate = match &command {
                Command::Repair {} => Some(backend.maintenance.gate.clone()),
                Command::Backup { kind } => Some(backend.maintenance.backup_gate(*kind)),
                _ => None,
            };
            let changes_files = !matches!(
                command,
                Command::Backup { .. }
                    | Command::BackupSchedule { .. }
                    | Command::Inspect { .. }
                    | Command::Export { .. }
            );
            blocking(backend, changes_files, gate, move |backend| {
                command.run(backend)
            })
            .await
        }
        Err(error) => (error.status(), Json(json!({"error":error.body_text()}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_waiters_leave_blocking_pool_available_to_other_kinds() -> Result<()> {
        use crate::backup::{Kind, Schedule};

        let root = tempfile::tempdir()?;
        let engine = Engine::open(&root.path().join("metadata"))?;
        for kind in [Kind::Export, Kind::Metadata] {
            let path = root.path().join(format!("{}-backups", kind.as_str()));
            std::fs::create_dir_all(&path)?;
            engine.set_backup_schedule(
                kind,
                &Schedule {
                    enabled: false,
                    directories: vec![path],
                    seconds: 3600,
                    keep: 1,
                },
            )?;
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let backend = Backend {
                engine: engine.clone(),
                maintenance: Arc::new(Maintenance::new(engine.clone())),
                limiter: Arc::new(tokio::sync::Semaphore::new(2)),
            };
            // Background exports acquire this same gate before entering the pool.
            let active_export = backend
                .maintenance
                .backup_gate(Kind::Export)
                .lock_owned()
                .await;
            let pending = tokio::spawn(execute(
                State(backend.clone()),
                Ok(Json(Command::Backup { kind: Kind::Export })),
            ));
            let timeout = std::time::Duration::from_secs(3);
            tokio::time::timeout(timeout, async {
                while backend.limiter.available_permits() == 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            let metadata = tokio::time::timeout(
                timeout,
                execute(
                    State(backend),
                    Ok(Json(Command::Backup {
                        kind: Kind::Metadata,
                    })),
                ),
            )
            .await;
            let waiting = !pending.is_finished();
            drop(active_export);
            let export = tokio::time::timeout(timeout, pending).await??;
            assert!(waiting, "duplicate export bypassed its gate");
            assert_eq!(metadata?.status(), StatusCode::OK);
            assert_eq!(export.status(), StatusCode::OK);
            Ok(())
        })
    }

    #[test]
    fn blocking_limiter_bounds_queue_and_shutdown_drains_work() -> Result<()> {
        let root = tempfile::tempdir()?;
        let engine = Engine::open(&root.path().join("metadata"))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let maintenance = Arc::new(Maintenance::new(engine.clone()));
            let server = start(
                "127.0.0.1:0".parse()?,
                engine.clone(),
                maintenance.clone(),
                3,
            )
            .await?;
            let backend = Backend {
                engine,
                maintenance,
                limiter: server.limiter.clone(),
            };
            let (started, ready) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            let first = tokio::spawn(blocking(backend.clone(), false, None, move |_| {
                let _ = started.send(());
                released.recv()?;
                Ok(json!({}))
            }));
            ready.await?;
            // Fill the configured request limit while the sole blocking thread
            // is busy; further work is rejected rather than queued forever.
            let queued = backend.limiter.clone().acquire_many_owned(2).await?;
            let rejected = blocking(backend, false, None, |_| Ok(json!({}))).await;
            assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
            drop(queued);
            let mut shutdown = tokio::spawn(server.shutdown());
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(30), &mut shutdown)
                    .await
                    .is_err()
            );
            release.send(())?;
            assert_eq!(first.await?.status(), StatusCode::OK);
            tokio::time::timeout(std::time::Duration::from_secs(3), shutdown).await???;
            Ok(())
        })
    }

    #[test]
    fn admin_routes_enforce_origin_host_and_typed_commands() -> Result<()> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("metadata.db");
        let engine = Engine::open(&db)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let server = runtime.block_on(start(
            "127.0.0.1:0".parse()?,
            engine.clone(),
            Arc::new(Maintenance::new(engine)),
            2,
        ))?;
        let origin = format!("http://{}", server.addr);
        let api = format!("{origin}/api/action");
        let request = ureq::post(&api).set("Content-Type", "application/json");
        let index = ureq::get(&origin).call()?.into_string()?;
        for (asset, mime) in [
            ("/assets/app.js", "text/javascript"),
            ("/assets/app.css", "text/css"),
        ] {
            assert!(index.contains(&format!("\"{asset}\"")));
            let response = ureq::get(&format!("{origin}{asset}")).call()?;
            assert!(response.header("Content-Type").unwrap().starts_with(mime));
            assert!(!response.into_string()?.trim().is_empty());
        }
        // A slow request body must not serialize unrelated reads.
        use std::io::Write;
        let mut slow = std::net::TcpStream::connect(server.addr)?;
        write!(
            slow,
            "POST /api/action HTTP/1.1\r\nHost: {}\r\nOrigin: {origin}\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n{{",
            server.addr
        )?;
        let state = ureq::get(&format!("{origin}/api/state"))
            .timeout(std::time::Duration::from_secs(3))
            .call()?
            .into_string()?;
        drop(slow);
        assert!(serde_json::from_str::<Value>(&state)?["targets"].is_array());
        let page: Value = serde_json::from_reader(
            ureq::get(&format!("{origin}/api/trash?page=2&page_size=1"))
                .call()?
                .into_reader(),
        )?;
        assert_eq!(page["page"], 1);
        assert_eq!(page["page_size"], 1);
        assert_eq!(page["items"], json!([]));
        assert!(matches!(
            ureq::get(&format!("{origin}/api/trash?page_size=101")).call(),
            Err(ureq::Error::Status(400, _))
        ));
        assert!(matches!(
            ureq::get(&origin).set("Host", "evil.invalid").call(),
            Err(ureq::Error::Status(403, _))
        ));
        assert!(matches!(
            request
                .clone()
                .set("Origin", "http://other")
                .send_string("{}"),
            Err(ureq::Error::Status(403, _))
        ));
        let post = |action: &str, input: Value, expected: u16| -> Result<ureq::Response> {
            let result = request
                .clone()
                .set("Origin", &origin)
                .send_string(&json!({"action":action,"input":input}).to_string());
            let response = match result {
                Ok(response) | Err(ureq::Error::Status(_, response)) => response,
                Err(error) => return Err(error.into()),
            };
            assert_eq!(response.status(), expected);
            Ok(response)
        };
        let target = root.path().join("disk");
        std::fs::create_dir(&target)?;
        post("target", json!({"name":"disk","path":target}), 200)?;
        post(
            "policy",
            json!({"path":"/","write_cache":["disk"],"persistent_storage":["disk"],"persistent_copies":1,"persist_delay_secs":0}),
            200,
        )?;
        let path = root.path().join("files.csv");
        post("export", json!({"path":path}), 200)?;
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "logical_path,physical_path\n/,\n"
        );
        let export_dir = root.path().join("exports");
        let metadata_dir = root.path().join("backups");
        for (kind, directory) in [("export", &export_dir), ("metadata", &metadata_dir)] {
            std::fs::create_dir(directory)?;
            post(
                "backup_schedule",
                json!({"kind":kind,"enabled":false,"directories":[directory],"seconds":3600,"keep":2}),
                200,
            )?;
            post("backup", json!({"kind":kind}), 200)?;
        }
        let state: Value = serde_json::from_reader(
            ureq::get(&format!("{origin}/api/state"))
                .call()?
                .into_reader(),
        )?;
        assert_eq!(state["backup_schedules"].as_array().unwrap().len(), 2);
        assert_eq!(state["backup_status"].as_array().unwrap().len(), 2);
        let report = post(
            "inspect",
            json!({"path":metadata_dir,"roots":[target]}),
            200,
        )?
        .into_string()?;
        assert_eq!(
            serde_json::from_str::<Value>(&report)?["verified"],
            json!([])
        );
        post("purge", json!({"id":"wrong"}), 422)?;
        post("unknown", json!({}), 422)?;
        post("repair", json!({"payload":"a".repeat(65536)}), 413)?;
        post("repair", json!({}), 200)?;
        runtime.block_on(server.shutdown())?;
        Ok(())
    }
}
