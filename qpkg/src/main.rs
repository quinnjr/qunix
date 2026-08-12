mod artifact;
mod aur;
mod build;
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

use error::Result;
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
        cli::Command::Build { name, no_rewrite } => {
            let artifact = build::run(&index, &name, no_rewrite, now_unix())?;
            println!("built {}", artifact.display());
            Ok(())
        }
        cli::Command::Update { build: rebuild } => {
            let report = sync::run(&index, &sync::SyncConfig::default(), now_unix())?;
            eprintln!("synced {} official and {} AUR packages", report.official, report.aur);
            let names = build::outdated(&index, &mut out)?;
            if names.is_empty() {
                println!("everything built is current");
            } else if rebuild {
                for name in names {
                    let artifact = build::run(&index, &name, false, now_unix())?;
                    println!("rebuilt {}", artifact.display());
                }
            }
            Ok(())
        }
    }
}
