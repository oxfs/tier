mod config;
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{collections::BTreeMap, fs, net::SocketAddr, path::PathBuf};
use tier::{
    archive,
    backup::{self, Kind},
    core::Engine,
    recovery, service,
};

fn backup_kind(value: &str) -> std::result::Result<Kind, String> {
    serde_json::from_value(serde_json::json!(value)).map_err(|_| "use export or metadata".into())
}

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Startup TOML (default: $XDG_CONFIG_HOME/tier/tier.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Override the metadata directory (default: $XDG_STATE_HOME/tier/metadata).
    #[arg(long, global = true)]
    db: Option<PathBuf>,
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
    /// Configure a file export or metadata backup schedule (JSON).
    Schedule {
        #[arg(value_parser = backup_kind)]
        kind: Kind,
        json: String,
    },
    /// Run the configured export or metadata backup immediately.
    Backup {
        #[arg(value_parser = backup_kind)]
        kind: Kind,
    },
    /// List the available metadata backups in a backup directory.
    Backups {
        directory: PathBuf,
    },
    /// Run the Samba IPC service, background workers and administration UI.
    Serve {
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        admin: Option<SocketAddr>,
    },
    /// Run one manual repair and archive pass.
    Repair,
    /// Export logical-to-physical file mappings as a two-column CSV.
    Export {
        output: PathBuf,
    },
    /// Check or extract files using a metadata backup and read-only target roots.
    Inspect {
        directory: PathBuf,
        #[arg(long)]
        backup_id: Option<u32>,
        targets: Vec<PathBuf>,
        #[arg(long)]
        extract: Option<PathBuf>,
    },
    /// Restore metadata into a new database. Map relocated targets with NAME=PATH.
    Restore {
        directory: PathBuf,
        #[arg(long)]
        backup_id: Option<u32>,
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
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .from_env()?;
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(filter)
        .init();
    let cli = Cli::parse();
    if let Action::Backups { directory } = &cli.action {
        println!(
            "{}",
            serde_json::to_string_pretty(&backup::list(directory)?)?
        );
        return Ok(());
    }
    if let Action::Inspect {
        directory,
        backup_id,
        targets,
        extract,
    } = &cli.action
    {
        ensure!(
            extract.is_none() || !targets.is_empty(),
            "extraction requires a local storage target"
        );
        let report = recovery::inspect_backup(directory, *backup_id, targets, extract.as_deref())?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let settings = config::Settings::load(cli.config.as_deref())?;
    let db = settings.database(cli.db)?;
    if let Action::Restore {
        directory,
        backup_id,
        targets,
        allow_degraded,
    } = &cli.action
    {
        let mut map = BTreeMap::new();
        for mapping in targets {
            let (name, path) = mapping.split_once('=').context("use NAME=PATH")?;
            map.insert(name.to_owned(), path.into());
        }
        let report = recovery::restore_backup(
            directory,
            *backup_id,
            &db,
            &map,
            *allow_degraded,
            &settings.rocksdb,
        )?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let mut lock_name = db.as_os_str().to_owned();
    lock_name.push(".lock");
    let guard = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_name)?;
    guard.try_lock().context(
        "database is in use; manage a running Tier instance through its local web interface",
    )?;
    let engine = Engine::open_with_options(&db, &settings.rocksdb)?;
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
        Action::ArchivePolicy { path, json } => engine.set_archive_policy(
            &path,
            &serde_json::from_str::<archive::ArchivePolicy>(&json)?,
        ),
        Action::Inherit { path, archive } => engine.inherit_policy(&path, archive),
        Action::Schedule { kind, json } => {
            engine.set_backup_schedule(kind, &serde_json::from_str(&json)?)
        }
        Action::Backup { kind } => engine.backup_tick(kind),
        Action::Export { output } => tier::export::write(&engine, &output),
        Action::Repair => service::repair(&engine),
        Action::Status => {
            println!("{}", serde_json::to_string_pretty(&engine.state()?)?);
            Ok(())
        }
        Action::Mkdir { path } => engine.mkdir(&path),
        Action::Serve { socket, admin } => service::serve(
            engine,
            &socket
                .or(settings.socket)
                .unwrap_or_else(|| db.with_extension("sock")),
            admin
                .or(settings.admin)
                .unwrap_or_else(|| ([127, 0, 0, 1], 9480).into()),
            &settings.runtime,
            &settings.ipc,
        ),
        Action::Restore { .. } | Action::Inspect { .. } | Action::Backups { .. } => unreachable!(),
    }
}
