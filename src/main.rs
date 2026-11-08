use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    collections::BTreeMap,
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tier::{admin, cloud, core::Engine, mount, recovery};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, global = true, default_value = "tier.sqlite")]
    db: PathBuf,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Register a local target. Its existing directory receives a persistent identity.
    Target {
        name: String,
        path: PathBuf,
        #[arg(long)]
        allow_same_device: bool,
        #[arg(long)]
        domain: Option<String>,
    },
    /// Bind a storage policy (JSON) to a namespace directory.
    Policy {
        path: String,
        json: String,
    },
    /// Register an existing WebDAV collection. Credentials refer to environment variable names.
    Archive {
        name: String,
        url: String,
        #[arg(long)]
        user_env: Option<String>,
        #[arg(long)]
        pass_env: Option<String>,
    },
    ArchivePolicy {
        path: String,
        json: String,
    },
    /// Set the metadata snapshot schedule (JSON).
    Schedule {
        json: String,
    },
    /// Run the filesystem, background worker and local administration UI.
    Serve {
        mountpoint: PathBuf,
        #[arg(long, default_value = "127.0.0.1:9480")]
        admin: SocketAddr,
    },
    /// Run one manual repair and archive pass.
    Repair,
    Snapshot {
        output: PathBuf,
    },
    /// Check or extract snapshot objects from the supplied read-only target roots.
    Inspect {
        snapshot: PathBuf,
        targets: Vec<PathBuf>,
        #[arg(long)]
        extract: Option<PathBuf>,
    },
    /// Load a snapshot into a new database. Map relocated targets with NAME=PATH.
    Import {
        snapshot: PathBuf,
        #[arg(long = "target")]
        targets: Vec<String>,
        /// Accept the reported offline, damaged, missing or unclaimed data.
        #[arg(long)]
        allow_degraded: bool,
    },
    Status,
    Mkdir {
        path: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Action::Inspect {
        snapshot,
        targets,
        extract,
    } = &cli.action
    {
        let data = recovery::load(snapshot)?;
        let report = match extract {
            Some(out) => {
                ensure!(
                    !targets.is_empty(),
                    "extraction requires a local storage target"
                );
                recovery::inspect(&data, targets, Some(out))?
            }
            None => recovery::preflight(&data, targets)?,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if let Action::Import {
        snapshot,
        targets,
        allow_degraded,
    } = &cli.action
    {
        let mut map = BTreeMap::new();
        for mapping in targets {
            let (name, path) = mapping.split_once('=').context("use NAME=PATH")?;
            map.insert(name.to_owned(), path.into());
        }
        let report = recovery::import(&recovery::load(snapshot)?, &cli.db, &map, *allow_degraded)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let mut lock_name = cli.db.as_os_str().to_owned();
    lock_name.push(".lock");
    let guard = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_name)?;
    guard.try_lock().context(
        "database is in use; manage a running Tier instance through its local web interface",
    )?;
    let engine = Engine::open(&cli.db)?;
    match cli.action {
        Action::Target {
            name,
            path,
            allow_same_device,
            domain,
        } => engine.register_target(&name, &path, domain.as_deref(), allow_same_device),
        Action::Policy { path, json } => engine.set_policy(&path, &serde_json::from_str(&json)?),
        Action::Archive {
            name,
            url,
            user_env,
            pass_env,
        } => engine.add_archive(&name, &url, user_env.as_deref(), pass_env.as_deref()),
        Action::ArchivePolicy { path, json } => {
            engine.set_cloud_policy(&path, &serde_json::from_str::<cloud::CloudPolicy>(&json)?)
        }
        Action::Schedule { json } => engine.schedule(&serde_json::from_str(&json)?),
        Action::Snapshot { output } => recovery::write_snapshot(&engine, &output),
        Action::Repair => {
            engine.reconcile()?;
            engine.archive_tick()?;
            engine.backup_tick()
        }
        Action::Status => {
            println!("{}", serde_json::to_string_pretty(&engine.state()?)?);
            Ok(())
        }
        Action::Mkdir { path } => engine.mkdir(&path),
        Action::Serve { mountpoint, admin } => serve(engine, &cli.db, &mountpoint, admin),
        Action::Import { .. } | Action::Inspect { .. } => unreachable!(),
    }
}
fn serve(engine: Engine, db: &Path, path: &Path, addr: SocketAddr) -> Result<()> {
    ensure!(path.is_dir(), "mountpoint must be an existing directory");
    let background = Engine::open(db)?;
    let snapshots = Engine::open(db)?;
    let engine = Arc::new(Mutex::new(engine));
    let panel = Arc::new(Mutex::new(Engine::open(db)?));
    let address = admin::start(addr, panel)?;
    eprintln!("Tier administration: http://{address}");
    thread::spawn(move || {
        loop {
            {
                let _worker = admin::WORKER.lock().expect("repair worker panicked");
                if let Err(e) = background.automatic_reconcile() {
                    eprintln!("storage: {e:#}")
                }
                if let Err(e) = background.archive_tick() {
                    eprintln!("archive: {e:#}")
                }
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
    thread::spawn(move || {
        loop {
            if let Err(e) = snapshots.schedule_backups() {
                eprintln!("snapshot: {e:#}")
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
    mount::mount(engine, path)
}
