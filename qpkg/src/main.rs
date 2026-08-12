mod cli;
mod error;
mod paths;

use clap::Parser;

use error::{Error, Result};

fn main() {
    if let Err(e) = run() {
        eprintln!("qpkg: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = cli::Cli::parse();
    match cli.command {
        cli::Command::Sync
        | cli::Command::Search { .. }
        | cli::Command::Info { .. }
        | cli::Command::Build { .. }
        | cli::Command::Update { .. } => Err(Error::Index(
            "this build of qpkg predates its command wiring".into(),
        )),
    }
}
