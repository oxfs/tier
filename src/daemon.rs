use anyhow::Result;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tier::{Store, config::Config};

pub fn run(path: &Path) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone())?;
    let health = crate::web::start(path, stop.clone())?;
    let mut last_audit = None;
    let mut last_archive_audit = None;
    while !stop.load(Ordering::Relaxed) {
        let mut interval = Duration::from_secs(60);
        let result = (|| -> Result<()> {
            // A changed backup takes effect on the next pass, without restart.
            let config = Config::load(path)?;
            interval = Duration::from_secs(config.background.scan_interval_secs);
            let audit_interval = Duration::from_secs(config.background.audit_interval_secs);
            let archive_audit_interval =
                Duration::from_secs(config.background.archive_audit_interval_secs);
            let store = Store::new(config)?;
            let report = store.scan_pending()?;
            crate::web::record(&health, "replicate", &serde_json::to_value(&report)?);
            log("replicate", &serde_json::to_value(report)?);
            let remote = last_archive_audit
                .is_none_or(|instant: Instant| instant.elapsed() >= archive_audit_interval);
            if remote
                || last_audit.is_none_or(|instant: Instant| instant.elapsed() >= audit_interval)
            {
                let report = if remote {
                    store.scan()?
                } else {
                    store.scan_local_audit()?
                };
                if remote {
                    last_archive_audit = Some(Instant::now());
                    crate::web::record(&health, "audit", &serde_json::to_value(&report)?);
                }
                let event = if remote { "archive_audit" } else { "audit" };
                crate::web::record(&health, event, &serde_json::to_value(&report)?);
                log(event, &serde_json::to_value(report)?);
                last_audit = Some(Instant::now());
            }
            Ok(())
        })();
        if let Err(error) = result {
            crate::web::record(
                &health,
                "error",
                &serde_json::json!({"message": format!("{error:#}")}),
            );
            log(
                "error",
                &serde_json::json!({"message": format!("{error:#}")}),
            );
        }
        let deadline = Instant::now() + interval;
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(())
}

fn log(event: &str, details: &serde_json::Value) {
    eprintln!(
        "{}",
        serde_json::json!({
            "time": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            "event": event,
            "details": details,
        })
    );
}
