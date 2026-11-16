use anyhow::{Result, ensure};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Child, Command, Stdio},
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

fn wait_until(mut condition: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !condition() {
        ensure!(
            Instant::now() < deadline,
            "background service did not make progress"
        );
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

#[test]
fn foreground_interrupts_at_file_boundaries_and_next_round_retries() -> Result<()> {
    use fs2::FileExt;
    let root = tempfile::tempdir()?;
    let primary = root.path().join("primary");
    let replica = root.path().join("replica");
    fs::create_dir(&primary)?;
    fs::create_dir(&replica)?;
    let config = root.path().join("config.toml");
    fs::write(
        &config,
        format!(
            "primary={primary:?}\n[replicas]\na={replica:?}\n[[backup]]\npath=''\ncount=1\ngc=true\n[background]\nworkers=2\nscan_interval_secs=1\naudit_interval_secs=3600\narchive_audit_interval_secs=3600\naudit_bytes_per_sec=262144\n"
        ),
    )?;
    let store = Store::initialize(Config::load(&config)?)?;
    store.mkdir(Path::new("source"))?;
    for index in 0..5 {
        store
            .write(Path::new(&format!("source/{index}")))?
            .write_all(&[42; 128 * 1024])?;
    }
    assert_eq!(store.scan_pending()?.changed, 5);
    let log = root.path().join("daemon.log");
    let _service = Service(
        Command::new(env!("CARGO_BIN_EXE_tier"))
            .arg(&config)
            .env_remove("TIER_WEB_LISTEN")
            .stderr(fs::File::create(&log)?)
            .spawn()?,
    );
    // Observe both workers through the actual cross-process descriptor locks.
    wait_until(|| {
        (0..2).all(|index| {
            let file = fs::File::open(primary.join(format!(".tier/meta/source/{index}"))).unwrap();
            FileExt::try_lock_shared(&file)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)
        })
    })?;
    let started = Instant::now();
    assert!(
        store
            .rename(Path::new("source"), Path::new("moved"))
            .is_err()
    );
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "foreground waited for audit"
    );
    let reports = || -> Vec<serde_json::Value> {
        fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|r| r["event"] == "archive_audit")
            .collect()
    };
    wait_until(|| !reports().is_empty())?;
    let first = reports().remove(0);
    assert_eq!(first["details"]["interrupted"], true);
    assert_eq!(first["details"]["completed"], 2);
    assert_eq!(first["details"]["errors"], serde_json::json!([]));
    store.rename(Path::new("source"), Path::new("moved"))?;
    // Interrupted audits must not advance the weekly/monthly completion clocks.
    wait_until(|| {
        reports()
            .iter()
            .any(|r| r["details"]["interrupted"] == false)
    })?;
    let final_report = reports().pop().unwrap();
    assert_eq!(final_report["details"]["completed"], 5);
    assert_eq!(final_report["details"]["errors"], serde_json::json!([]));
    for index in 0..5 {
        let relative = format!("moved/{index}");
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut store.read(Path::new(&relative))?, &mut bytes)?;
        assert_eq!(bytes, vec![42; 128 * 1024]);
        store.remove(Path::new(&relative), false)?;
    }
    store.remove(Path::new("moved"), true)?;
    for disk in [&primary, &replica] {
        assert!(!disk.join("moved").exists());
        assert!(!disk.join(".tier/meta/moved").exists());
    }
    Ok(())
}

#[test]
fn service_skips_busy_files_reloads_backup_and_stops_cleanly() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let primary = directory.path().join("primary");
    let replica = directory.path().join("replica");
    let second = directory.path().join("second");
    fs::create_dir(&primary)?;
    fs::create_dir(&replica)?;
    fs::create_dir(&second)?;
    let config = directory.path().join("config.toml");
    let settings = |gc| {
        format!(
            "primary = '{}'\n[replicas]\nbackup = '{}'\nsecond = '{}'\n[[backup]]\npath = ''\ncount = 2\ngc = {gc}\n[background]\nscan_interval_secs = 1\naudit_interval_secs = 2\n",
            primary.display(),
            replica.display(),
            second.display()
        )
    };
    fs::write(&config, settings(false))?;
    let store = Store::initialize(Config::load(&config)?)?;
    {
        let mut writer = store.write(Path::new("file"))?;
        writer.write_all(b"durable content")?;
        writer.sync_all()?;
    }
    let reader = store.read(Path::new("file"))?;
    let log_path = directory.path().join("service.log");
    let mut service = Service(
        Command::new(env!("CARGO_BIN_EXE_tier"))
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log_path)?)
            .spawn()?,
    );
    wait_until(|| fs::read_to_string(&log_path).is_ok_and(|log| log.contains("replicate")))?;
    assert!(!replica.join("file").exists());
    drop(reader);
    wait_until(|| replica.join("file").exists() || second.join("file").exists())?;
    let selected = if replica.join("file").exists() {
        &replica
    } else {
        &second
    };
    assert_eq!(fs::read(selected.join("file"))?, b"durable content");
    // Atomic configuration replacement, as a deployment tool would perform.
    let replacement = directory.path().join("next.toml");
    fs::write(&replacement, settings(true))?;
    fs::rename(replacement, &config)?;
    wait_until(|| !primary.join("file").exists())?;
    assert_eq!(fs::read(second.join("file"))?, b"durable content");
    // SAFETY: child id is a live child process started above; SIGTERM is valid.
    ensure!(
        unsafe { libc::kill(service.0.id() as libc::pid_t, libc::SIGTERM) } == 0,
        "failed to stop service"
    );
    wait_until(|| service.0.try_wait().is_ok_and(|status| status.is_some()))?;
    ensure!(service.0.wait()?.success(), "service did not exit cleanly");
    Ok(())
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
