use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{collections::BTreeMap, fs, net::SocketAddr, path::PathBuf};
use tier::{archive, core::Engine, recovery, service};

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
    /// Remove a directory override and resume inheritance from its parent.
    Inherit {
        path: String,
        #[arg(long)]
        archive: bool,
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
        let data = tier::snapshot::load(snapshot)?;
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
        let report = recovery::import(
            &tier::snapshot::load(snapshot)?,
            &cli.db,
            &map,
            *allow_degraded,
        )?;
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
        Action::ArchivePolicy { path, json } => engine.set_cloud_policy(
            &path,
            &serde_json::from_str::<archive::ArchivePolicy>(&json)?,
        ),
        Action::Inherit { path, archive } => engine.inherit_policy(&path, archive),
        Action::Schedule { json } => engine.schedule(&serde_json::from_str(&json)?),
        Action::Snapshot { output } => tier::snapshot::write(&engine, &output),
        Action::Repair => {
            service::repair(&engine)?;
            engine.backup_tick()
        }
        Action::Status => {
            println!("{}", serde_json::to_string_pretty(&engine.state()?)?);
            Ok(())
        }
        Action::Mkdir { path } => engine.mkdir(&path),
        Action::Serve { mountpoint, admin } => service::serve(engine, &cli.db, &mountpoint, admin),
        Action::Import { .. } | Action::Inspect { .. } => unreachable!(),
    }
}
