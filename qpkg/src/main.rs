mod aur;
mod cli;
mod commands;
mod error;
mod index;
mod paths;
mod pkgbuild;
mod repodb;
mod runner;
mod sources;
mod sync;
mod toolchain;
#[cfg(test)]
mod testutil;
mod vercmp;

use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;

use error::{Error, Result};
use index::Index;

fn main() {
    if let Err(e) = run() {
        eprintln!("qpkg: {e}");
        std::process::exit(1);
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn run() -> Result<()> {
    let cli = cli::Cli::parse();
    let index = Index::open(&cli.db.unwrap_or_else(paths::db_path))?;
    let mut out = std::io::stdout();
    let mut warn = std::io::stderr();
    match cli.command {
        cli::Command::Sync => {
            let report = sync::run(&index, &sync::SyncConfig::default(), now_unix())?;
            println!("synced {} official and {} AUR packages", report.official, report.aur);
            Ok(())
        }
        cli::Command::Search { term } => {
            commands::search(&index, &term, &mut out, &mut warn, now_unix())
        }
        cli::Command::Info { name } => {
            commands::info(&index, &name, &mut out, &mut warn, now_unix())
        }
        cli::Command::Build { .. } | cli::Command::Update { .. } => Err(Error::Index(
            "this build of qpkg predates its build pipeline".into(),
        )),
    }
}
