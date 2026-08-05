//! Builds the userspace programs the kernel boots.
//!
//! Assembled here rather than in the kernel's build script because the result
//! is not linked into the kernel: it is a separate ELF the bootloader loads as
//! a module, and only `xtask` knows where the ESP is.
//!
//! Requires clang and LLD, which this project already requires everywhere else
//! (see `.cargo/config.toml`), so it adds no new dependency.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Assembles `kernel/user/init.s` into a static ELF64 executable.
///
/// Returns the path to the built binary. A real ELF rather than a flat image:
/// the point of the loader is to parse one, and a flat binary would leave that
/// path exercised only by unit tests.
pub fn build_init(root: &Path, target_dir: &Path) -> Result<PathBuf> {
    let src = root.join("kernel/user/init.s");
    let out = target_dir.join("userland");
    std::fs::create_dir_all(&out)?;

    let obj = out.join("init.o");
    run(
        Command::new("clang")
            .args(["-target", "x86_64-unknown-none", "-nostdlib", "-c", "-o"])
            .arg(&obj)
            .arg(&src),
        "clang",
    )?;

    let elf = out.join("init.elf");
    run(
        Command::new("ld.lld")
            // `-Ttext` fixes the load address, and `--build-id=none` keeps the
            // output byte-identical across builds so a rebuilt ESP does not
            // look changed when nothing was.
            .args(["-Ttext=0x400000", "-e", "_start", "--build-id=none", "-o"])
            .arg(&elf)
            .arg(&obj),
        "ld.lld",
    )?;

    Ok(elf)
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to run {what}; is it installed?"))?;
    if !status.success() {
        bail!("{what} failed while building the init binary");
    }
    Ok(())
}
