#![cfg_attr(not(any(test, feature = "std")), no_std)]

//! The VFS's decisions, without its I/O.
//!
//! Which component a path names, which descriptor a table hands out, and what
//! an `lseek` resolves to are all answerable without touching a filesystem, so
//! they are answered here and tested on the host. `kernel::vfs` owns what is
//! actually on the disk.
//!
//! `fd` and `seek` are added in later tasks; this task is path decomposition
//! only, so only `path` is declared here.

pub mod path;

pub use path::{Component, Path, PathError};
