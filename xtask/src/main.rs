use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Flags that make the bare-metal target buildable. Passed per-invocation
/// rather than living in `.cargo/config.toml`, because `[unstable] build-std`
/// is global and would break the host-targeted `xtask` build.
const BUILD_STD: &[&str] = &[
    "-Zbuild-std=core,compiler_builtins,alloc",
    "-Zbuild-std-features=compiler-builtins-mem",
    // Required since nightly-2026-07: JSON target specs are gated.
    "-Zjson-target-spec",
];

fn build_kernel(release: bool) -> Result<PathBuf> {
    let mut cmd = Command::new(env!("CARGO"));
    cmd.current_dir(workspace_root());
    cmd.args(["build", "--package", "qunix-kernel"]);
    cmd.args(BUILD_STD);
    if release {
        cmd.arg("--release");
    }
    let status = cmd.status().context("failed to invoke cargo build")?;
    if !status.success() {
        bail!("kernel build failed");
    }
    let profile = if release { "release" } else { "debug" };
    Ok(workspace_root()
        .join("target/x86_64-qunix-kernel")
        .join(profile)
        .join("qunix-kernel"))
}

mod image;
mod qemu;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let release = args.iter().any(|a| a == "--release");
    let root = workspace_root();

    match args.first().map(String::as_str) {
        Some("build") => {
            let elf = build_kernel(release)?;
            println!("kernel: {}", elf.display());
            Ok(())
        }
        Some("run") => {
            let elf = build_kernel(release)?;
            let esp = image::build_esp(&root, &elf)?;
            let code = qemu::run_esp(&esp, false)?;
            std::process::exit(code);
        }
        other => bail!("unknown xtask command: {other:?}"),
    }
}
