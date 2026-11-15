use anyhow::{Result, ensure};
use std::path::Path;

mod daemon;
mod hardware;
mod web;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() == 2 && args[1] == "--version" {
        println!("tier {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.len() == 2 && args[1] == "--hardware" {
        return hardware::run();
    }
    ensure!(args.len() == 2, "usage: tier CONFIG | --hardware");
    daemon::run(Path::new(&args[1]))
}
