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
        Some("runner") => {
            // Invoked by cargo as the custom-target runner, with the test ELF path.
            let elf = PathBuf::from(args.get(1).context("runner requires an ELF path")?);
            let esp = image::build_esp(&root, &elf)?;
            match qemu::run_esp(&esp, true)? {
                33 => Ok(()),                       // ExitCode::Success
                35 => bail!("kernel tests failed"), // ExitCode::Failure
                other => bail!("qemu exited with unexpected status {other}"),
            }
        }
        Some("test") => {
            let mut cmd = Command::new(env!("CARGO"));
            cmd.current_dir(&root);
            cmd.args(["test", "--package", "qunix-kernel"]);
            cmd.args(BUILD_STD);
            // Without this, cargo forces `panic=unwind` for test units, which
            // makes build-std compile `core` a second time and collide with the
            // panic=abort copy (E0152: duplicate lang item).
            cmd.arg("-Zpanic-abort-tests");
            if !cmd.status()?.success() {
                bail!("kernel tests failed");
            }
            // Host-testable crates are listed explicitly: `--features` is not
            // accepted at the root of a virtual workspace, and the HAL crate
            // cannot build for the host at all.
            for package in ["qunix-sync", "qunix-mm"] {
                let mut host = Command::new(env!("CARGO"));
                host.current_dir(&root);
                host.args([
                    "test",
                    "--target",
                    "x86_64-unknown-linux-musl",
                    "--package",
                    package,
                    "--features",
                    "std",
                ]);
                if !host.status()?.success() {
                    bail!("host tests failed for {package}");
                }
            }
            Ok(())
        }
        other => bail!("unknown xtask command: {other:?}"),
    }
}
