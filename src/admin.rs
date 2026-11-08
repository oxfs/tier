use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc};
use tiny_http::{Header, Method, Request, Response, Server};

pub trait Admin: Send + Sync {
    fn state(&self) -> Result<Value>;
    fn execute(&self, action: &str, input: Value) -> Result<Value>;
}

pub fn start(addr: SocketAddr, backend: Arc<dyn Admin>) -> Result<SocketAddr> {
    ensure!(addr.ip().is_loopback(), "admin listener must use loopback");
    let server = Server::http(addr).map_err(|error| anyhow::anyhow!("{error}"))?;
    let addr = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow::anyhow!("invalid admin listener"))?;
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            if let Err(error) = respond(request, addr, backend.as_ref()) {
                eprintln!("admin response: {error}");
            }
        }
    });
    Ok(addr)
}

fn respond(mut request: Request, addr: SocketAddr, backend: &dyn Admin) -> Result<()> {
    let header = |key: &str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(key))
            .map(|h| h.value.to_string())
    };
    let host = addr.to_string();
    if header("host").as_deref() != Some(host.as_str()) {
        request.respond(Response::empty(403))?;
        return Ok(());
    }
    let (code, mime, body) = match (request.method(), request.url()) {
        (&Method::Get, "/") => (
            200,
            "text/html; charset=utf-8",
            include_str!("admin.html").to_owned(),
        ),
        (&Method::Get, "/api/state") => json_result(backend.state()),
        (&Method::Post, "/api/action")
            if header("origin").as_deref() == Some(format!("http://{host}").as_str())
                && header("content-type").as_deref() == Some("application/json") =>
        {
            use std::io::Read;
            let mut data = Vec::new();
            request.as_reader().take(65537).read_to_end(&mut data)?;
            json_result((|| {
                ensure!(data.len() <= 65536, "request too large");
                let value: Value = serde_json::from_slice(&data)?;
                backend.execute(
                    value["action"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("missing action"))?,
                    value["input"].clone(),
                )
            })())
        }
        _ => (404, "text/plain", "Not found".to_owned()),
    };
    let content_type = Header::from_bytes("Content-Type", mime).unwrap();
    request.respond(
        Response::from_string(body)
            .with_status_code(code)
            .with_header(content_type),
    )?;
    Ok(())
}

fn json_result(result: Result<Value>) -> (u16, &'static str, String) {
    let (code, body) = match result {
        Ok(value) => (200, value),
        Err(error) => (400, json!({"error":error.to_string()})),
    };
    (code, "application/json", body.to_string())
}

impl Admin for std::sync::Mutex<crate::core::Engine> {
    fn state(&self) -> Result<Value> {
        self.lock()
            .map_err(|_| anyhow::anyhow!("engine unavailable"))?
            .state()
    }

    fn execute(&self, action: &str, input: Value) -> Result<Value> {
        use std::path::Path;
        let string = |key: &str| {
            input[key]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing {key}"))
        };
        let entry = || {
            input["id"]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("missing id"))
        };
        let engine = self
            .lock()
            .map_err(|_| anyhow::anyhow!("engine unavailable"))?;
        match action {
            "repair" => {
                engine.reconcile()?;
                engine.archive_tick()?;
                engine.backup_tick()?;
            }
            "restore" => engine.restore(entry()?, string("path")?)?,
            "purge" => engine.purge(entry()?)?,
            "target" => engine.register_target(
                string("name")?,
                Path::new(string("path")?),
                input["domain"].as_str(),
                input["allow_same_domain"].as_bool().unwrap_or(false),
            )?,
            "policy" => {
                engine.set_policy(string("path")?, &serde_json::from_value(input.clone())?)?
            }
            "archive" => engine.add_archive(
                string("name")?,
                string("url")?,
                input["user_env"].as_str().filter(|s| !s.is_empty()),
                input["pass_env"].as_str().filter(|s| !s.is_empty()),
            )?,
            "cloud_policy" => {
                engine.set_cloud_policy(string("path")?, &serde_json::from_value(input.clone())?)?
            }
            "backup_schedule" => engine.schedule(&serde_json::from_value(input)?)?,
            "snapshot" => {
                let destination = Path::new(string("path")?);
                ensure!(destination.is_absolute(), "use an absolute snapshot path");
                crate::recovery::write_snapshot(&engine, destination)?;
            }
            _ => anyhow::bail!("unknown action"),
        }
        Ok(json!({"ok":true}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    struct Demo;
    impl Admin for Demo {
        fn state(&self) -> Result<Value> {
            Ok(json!({"health": []}))
        }
        fn execute(&self, action: &str, input: Value) -> Result<Value> {
            Ok(json!([action, input]))
        }
    }

    #[test]
    fn loopback_api_checks_browser_origin() -> Result<()> {
        let server = Server::http("127.0.0.1:0").map_err(|e| anyhow::anyhow!("{e}"))?;
        let addr = server.server_addr().to_ip().unwrap();
        let worker = thread::spawn(move || {
            for req in server.incoming_requests().take(3) {
                respond(req, addr, &Demo).unwrap();
            }
        });
        let origin = format!("http://{addr}");
        let state = ureq::get(&format!("{origin}/api/state"))
            .call()?
            .into_string()?;
        assert_eq!(serde_json::from_str::<Value>(&state)?, json!({"health":[]}));
        let url = format!("{origin}/api/action");
        let request = ureq::post(&url).set("Content-Type", "application/json");
        assert!(matches!(
            request
                .clone()
                .set("Origin", "http://other")
                .send_string("{}"),
            Err(ureq::Error::Status(404, _))
        ));
        let result = request
            .set("Origin", &origin)
            .send_string(r#"{"action":"repair","input":{}}"#)?
            .into_string()?;
        assert_eq!(
            serde_json::from_str::<Value>(&result)?,
            json!(["repair", {}])
        );
        worker.join().unwrap();
        Ok(())
    }
    #[test]
    fn admin_snapshot_uses_recoverable_envelope() -> Result<()> {
        use crate::{core::Engine, recovery};
        use std::sync::Mutex;
        let tmp = tempfile::tempdir()?;
        let engine = Mutex::new(Engine::open(&tmp.path().join("metadata.sqlite"))?);
        let target = tmp.path().join("storage");
        std::fs::create_dir(&target)?;
        engine.execute("target", json!({"name":"ssd","path":target}))?;
        engine.execute("policy",json!({"path":"/","front":["ssd"],"front_count":1,"long":["ssd"],"long_count":1,"delay_secs":0}))?;
        let snapshot = tmp.path().join("metadata.json");
        engine.execute("snapshot", json!({"path":snapshot}))?;
        assert_eq!(recovery::load(&snapshot)?["targets"][0]["name"], "ssd");
        Ok(())
    }
}
