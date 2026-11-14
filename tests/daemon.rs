use anyhow::{Result, ensure};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};
use tier::{Store, config::Config};

struct Service(Child);

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn daemon_drains_changes_then_sleeps_without_reauditing() -> Result<()> {
    let root = tempfile::tempdir()?;
    let primary = root.path().join("primary");
    let replica = root.path().join("replica");
    fs::create_dir(&primary)?;
    fs::create_dir(&replica)?;
    let config = root.path().join("config.toml");
    fs::write(
        &config,
        format!(
            "primary={primary:?}\n[replicas]\na={replica:?}\n[[backup]]\npath=''\ncount=2\n[background]\nscan_interval_secs=3600\n"
        ),
    )?;
    let store = Store::initialize(Config::load(&config)?)?;
    store.write(Path::new("file"))?.write_all(b"contents")?;
    let log = root.path().join("daemon.log");
    let _service = Service(
        Command::new(env!("CARGO_BIN_EXE_tier"))
            .arg(&config)
            .env_remove("TIER_WEB_LISTEN")
            .stderr(fs::File::create(&log)?)
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let reports = loop {
        let reports: Vec<serde_json::Value> = fs::read_to_string(&log)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        if reports.iter().filter(|r| r["event"] == "replicate").count() >= 2 {
            break reports;
        }
        ensure!(Instant::now() < deadline, "daemon did not drain changes");
        thread::sleep(Duration::from_millis(20));
    };
    let copies: Vec<_> = reports
        .iter()
        .filter(|r| r["event"] == "replicate")
        .collect();
    assert_eq!(copies[0]["details"]["completed"], 1);
    assert_eq!(copies[0]["details"]["changed"], 1);
    assert_eq!(copies[1]["details"]["completed"], 0);
    assert_eq!(copies[1]["details"]["changed"], 0);
    let audits: Vec<_> = reports
        .iter()
        .filter(|r| r["event"] == "archive_audit")
        .collect();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0]["details"]["changed"], 1);
    assert!(store.check(Path::new("file"))?);
    thread::sleep(Duration::from_millis(250));
    assert_eq!(fs::read_to_string(log)?.lines().count(), reports.len());
    Ok(())
}
