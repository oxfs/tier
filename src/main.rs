use anyhow::{Result, ensure};
use std::path::Path;

mod daemon;
mod web;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() == 2 && args[1] == "--version" {
        println!("tier {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    ensure!(args.len() == 2, "usage: tier CONFIG");
    daemon::run(Path::new(&args[1]))
}
