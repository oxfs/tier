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
use std::{
    net::{SocketAddr, TcpListener},
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct Backend {
    engine: Arc<Mutex<Engine>>,
    maintenance: Arc<Maintenance>,
}

pub(crate) struct Server {
    pub(crate) addr: SocketAddr,
    pub(crate) runtime: tokio::runtime::Handle,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) fn start(
    addr: SocketAddr,
    engine: Engine,
    maintenance: Arc<Maintenance>,
) -> Result<Server> {
    ensure!(addr.ip().is_loopback(), "admin listener must use loopback");
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();
    let app = router(
        addr,
        Backend {
            engine: Arc::new(Mutex::new(engine)),
            maintenance,
        },
    );
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("tier-admin".into())
        .spawn(move || {
            let result = runtime.block_on(async {
                axum::serve(tokio::net::TcpListener::from_std(listener)?, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown.await;
                    })
                    .await
            });
            if let Err(error) = result {
                tracing::error!(%error, "administration server stopped");
            }
        })?;
    Ok(Server {
        addr,
        runtime: handle,
        stop: Some(stop),
        thread: Some(thread),
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

async fn blocking(operation: impl FnOnce() -> Result<Value> + Send + 'static) -> Response {
    match tokio::task::spawn_blocking(operation)
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
    blocking(move || {
        backend
            .engine
            .lock()
            .map_err(|_| anyhow::anyhow!("engine unavailable"))?
            .trash_page(query.page.unwrap_or(1), query.page_size.unwrap_or(25))
    })
    .await
}

async fn read_state(State(backend): State<Backend>) -> Response {
    blocking(move || {
        backend
            .engine
            .lock()
            .map_err(|_| anyhow::anyhow!("engine unavailable"))?
            .state()
    })
    .await
}
async fn execute(
    State(backend): State<Backend>,
    body: Result<Json<Command>, JsonRejection>,
) -> Response {
    match body {
        Ok(Json(command)) => blocking(move || command.run(&backend)).await,
        Err(error) => (error.status(), Json(json!({"error":error.body_text()}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_routes_enforce_origin_host_and_typed_commands() -> Result<()> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("metadata.db");
        let server = start(
            "127.0.0.1:0".parse()?,
            Engine::open(&db)?,
            Arc::new(Maintenance::new(&db)),
        )?;
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
            json!({"path":"/","write_cache":["disk"],"cache_copies":1,"persistent_storage":["disk"],"persistent_copies":1,"persist_delay_secs":0}),
            200,
        )?;
        let path = root.path().join("snapshot.json");
        post("snapshot", json!({"path":path}), 200)?;
        assert_eq!(crate::snapshot::load(&path)?["targets"][0]["name"], "disk");
        let report = post("inspect", json!({"path":path,"roots":[target]}), 200)?.into_string()?;
        assert_eq!(
            serde_json::from_str::<Value>(&report)?["verified"],
            json!([])
        );
        post("purge", json!({"id":"wrong"}), 422)?;
        post("unknown", json!({}), 422)?;
        post("repair", json!({"payload":"a".repeat(65536)}), 413)?;
        post("repair", json!({}), 200)?;
        Ok(())
    }
}
