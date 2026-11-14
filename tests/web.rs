use anyhow::Result;
use std::{
    fs,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

#[test]
fn management_survives_invalid_config_and_rejects_cross_origin_requests() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = directory.path().join("config.toml");
    fs::write(&config, "invalid configuration")?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    let mut child = Command::new(env!("CARGO_BIN_EXE_tier"))
        .arg(&config)
        .env("TIER_WEB_LISTEN", address.to_string())
        .stderr(Stdio::null())
        .spawn()?;
    let outcome = (|| -> Result<()> {
        let endpoint = format!("http://{address}/api/health");
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(2))
            .build();
        let mut response = None;
        for _ in 0..50 {
            if let Ok(value) = agent.get(&endpoint).set("X-Tier-Request", "1").call() {
                response = Some(value);
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let health: serde_json::Value = serde_json::from_reader(
            response
                .expect("management server did not start")
                .into_reader(),
        )?;
        assert_eq!(health["healthy"], false);
        assert!(health["error"].is_string());
        // An incomplete body must not hold a blocking worker or stop health requests.
        use std::io::{Read, Write};
        let mut slow = std::net::TcpStream::connect(address)?;
        slow.set_read_timeout(Some(Duration::from_secs(15)))?;
        write!(
            slow,
            "POST /api/config HTTP/1.1\r\nHost: {address}\r\nX-Tier-Request: 1\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{{"
        )?;
        assert!(
            agent
                .get(&endpoint)
                .set("X-Tier-Request", "1")
                .call()
                .is_ok()
        );
        let mut timeout_response = String::new();
        slow.read_to_string(&mut timeout_response)?;
        assert!(timeout_response.contains("request body timeout"));
        fs::remove_file(&config)?;
        let missing: serde_json::Value = serde_json::from_reader(
            agent
                .get(&endpoint)
                .set("X-Tier-Request", "1")
                .call()?
                .into_reader(),
        )?;
        assert_eq!(missing["healthy"], false);
        fs::write(
            &config,
            "primary='/primary'\n[replicas]\n[[backup]]\npath=''\ncount=1\n",
        )?;
        let config_url = format!("http://{address}/api/config");
        let current: serde_json::Value = serde_json::from_reader(
            agent
                .get(&config_url)
                .set("X-Tier-Request", "1")
                .call()?
                .into_reader(),
        )?;
        let updated = format!("# API saved\n{}", current["content"].as_str().unwrap());
        let body =
            serde_json::json!({"content": updated, "revision":current["revision"]}).to_string();
        agent
            .post(&format!("http://{address}/api/validate"))
            .set("X-Tier-Request", "1")
            .send_string(&body)?;
        assert!(!fs::read_to_string(&config)?.contains("API saved"));
        agent
            .post(&config_url)
            .set("X-Tier-Request", "1")
            .send_string(&body)?;
        assert_eq!(fs::read_to_string(&config)?, updated);
        assert!(
            agent
                .post(&config_url)
                .set("X-Tier-Request", "1")
                .send_string(&body)
                .is_err()
        );
        assert!(agent.get(&endpoint).call().is_err());
        assert!(
            agent
                .get(&endpoint)
                .set("X-Tier-Request", "1")
                .set("Host", "attacker.example")
                .call()
                .is_err()
        );
        let mut draining = std::net::TcpStream::connect(address)?;
        draining.set_read_timeout(Some(Duration::from_secs(15)))?;
        write!(
            draining,
            "POST /api/validate HTTP/1.1\r\nHost: {address}\r\nX-Tier-Request: 1\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{{"
        )?;
        thread::sleep(Duration::from_millis(200));
        // SIGTERM stops accepting but lets the bounded in-flight body finish.
        assert!(
            Command::new("kill")
                .arg("-TERM")
                .arg(child.id().to_string())
                .status()?
                .success()
        );
        thread::sleep(Duration::from_millis(200));
        assert!(child.try_wait()?.is_none());
        let mut drained = String::new();
        draining.read_to_string(&mut drained)?;
        assert!(drained.contains("request body timeout"));
        assert!(child.wait()?.success());
        Ok(())
    })();
    if child.try_wait()?.is_none() {
        child.kill()?;
    }
    child.wait()?;
    outcome
}
