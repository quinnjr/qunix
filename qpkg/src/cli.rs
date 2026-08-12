use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "qpkg", about = "Arch-sourced package pipeline for qunix")]
pub struct Cli {
    /// Path to the redb index (defaults to $QPKG_DB, then XDG data dir).
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Refresh the index from the configured mirror and the AUR metadata dump.
    Sync,
    /// Search the index offline by name or description.
    Search { term: String },
    /// Show indexed metadata for one package.
    Info { name: String },
    /// Fetch, rewrite, cross-build and package one package.
    Build {
        name: String,
        /// Skip the gcc→LLVM textual rewrite pass (environment injection
        /// still applies).
        #[arg(long)]
        no_rewrite: bool,
    },
    /// Re-sync, then report built packages whose upstream version moved.
    Update {
        /// Rebuild every outdated package after reporting it.
        #[arg(long)]
        build: bool,
    },
}
