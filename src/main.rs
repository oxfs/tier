use anyhow::{Context, Result, bail};
use std::{io, path::Path};
use tier::{Store, config::Config};

mod daemon;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() == 2 && args[1] == "--version" {
        println!("tier {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let usage = "usage: tier CONFIG {init|run|scan|read PATH|write PATH|sync PATH|check PATH}";
    let config = args.get(1).context(usage)?;
    let command = args.get(2).and_then(|arg| arg.to_str()).context(usage)?;
    if command == "init" && args.len() == 3 {
        Store::initialize(Config::load(Path::new(config))?)?;
        return Ok(());
    }
    if command == "run" && args.len() == 3 {
        return daemon::run(Path::new(config));
    }
    let store = Store::new(Config::load(Path::new(config))?)?;
    match command {
        "scan" if args.len() == 3 => {
            let report = store.scan()?;
            println!("{}", serde_json::to_string(&report)?);
            if !report.errors.is_empty() {
                bail!("scan reported errors");
            }
        }
        "read" | "write" | "sync" | "check" if args.len() == 4 => {
            let path = Path::new(&args[3]);
            match command {
                "read" => {
                    io::copy(&mut store.read(path)?, &mut io::stdout().lock())?;
                }
                "write" => {
                    let mut writer = store.write(path)?;
                    writer.set_len(0)?;
                    io::copy(&mut io::stdin().lock(), &mut writer)?;
                    writer.sync_all()?;
                }
                "sync" => {
                    if !store.reconcile(path)? {
                        bail!("file is busy");
                    }
                }
                _ => {
                    if !store.check(path)? {
                        bail!("file is busy");
                    }
                }
            }
        }
        _ => bail!(usage),
    }
    Ok(())
}
