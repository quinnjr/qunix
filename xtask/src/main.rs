use anyhow::{Context, Result, bail};
use qunix_abi::{HOST_STATUS_FAILURE, HOST_STATUS_SUCCESS};
use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Where cargo writes artifacts. `CARGO_TARGET_DIR` is common in CI and shared
/// build caches, and ignoring it yields a path that simply does not exist.
fn target_dir() -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        // `join` keeps an absolute value as-is. A relative one has to resolve
        // against the workspace root, because that is the `current_dir` every
        // cargo we spawn runs in -- xtask's own cwd is not necessarily the same.
        Some(dir) => workspace_root().join(dir),
        None => workspace_root().join("target"),
    }
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
    Ok(target_dir().join("x86_64-qunix-kernel").join(profile).join("qunix-kernel"))
}

mod attest;
mod coverage;
mod image;
mod licensing;
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
            let esp = image::build_esp(&root, &target_dir(), &elf)?;
            match qemu::run_esp(&esp, false)? {
                // A passing kernel exits QEMU with 33, which a shell would read
                // as failure; translate the harness statuses back into the
                // conventions a caller of `xtask run` actually expects.
                qemu::Exit::Code(HOST_STATUS_SUCCESS) => std::process::exit(0),
                qemu::Exit::Code(HOST_STATUS_FAILURE) => {
                    eprintln!("the kernel signalled failure via isa-debug-exit");
                    std::process::exit(1);
                }
                qemu::Exit::Code(code) => std::process::exit(code),
                // Shells report signal deaths as 128 + signo; mirroring that is
                // more useful than `exit(-1)`, which truncates to a bare 255.
                qemu::Exit::Signal(signo) => {
                    eprintln!("qemu was killed by signal {signo}");
                    std::process::exit(128 + signo);
                }
            }
        }
        Some("runner") => {
            // Invoked by cargo as the custom-target runner, with the test ELF path.
            let elf = PathBuf::from(args.get(1).context("runner requires an ELF path")?);
            let esp = image::build_esp(&root, &target_dir(), &elf)?;
            match qemu::run_esp(&esp, true)? {
                qemu::Exit::Code(HOST_STATUS_SUCCESS) => Ok(()),
                qemu::Exit::Code(HOST_STATUS_FAILURE) => bail!("kernel tests failed"),
                qemu::Exit::Code(other) => bail!("qemu exited with unexpected status {other}"),
                qemu::Exit::Signal(signo) => {
                    bail!("qemu was killed by signal {signo} before reporting a test result")
                }
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
            // The profiles differ behaviourally (`lto = "thin"`, and overflow
            // checks only in debug), so `--release` has to reach every child.
            if release {
                cmd.arg("--release");
            }
            if !cmd.status()?.success() {
                bail!("kernel tests failed");
            }
            // One invocation for all host-testable crates: three separate
            // cargo startups meant three dependency resolutions and no
            // cross-crate rustc parallelism. `pkg/feature` syntax is what makes
            // a multi-package selection able to enable per-package features.
            let mut host = Command::new(env!("CARGO"));
            host.current_dir(&root);
            host.args([
                "test",
                "--target",
                "x86_64-unknown-linux-musl",
                "-p",
                "qunix-sync",
                "-p",
                "qunix-mm",
                "-p",
                "qunix-hal-x86_64",
                "-p",
                "xtask",
                "--features",
                "qunix-sync/std,qunix-mm/std,qunix-hal-x86_64/std",
            ]);
            if release {
                host.arg("--release");
            }
            if !host.status()?.success() {
                bail!("host tests failed");
            }
            // Cheap, and it is the only thing standing between a future
            // Linux-compatibility crate and silently inheriting a permissive
            // licence from the workspace.
            licensing::check(&root)?;
            attest::check(&root, None)?;
            Ok(())
        }
        Some("attest") => attest::check(&root, args.get(1).map(String::as_str)),
        Some("coverage") => coverage::check(&root, args.iter().any(|a| a == "--update")),
        other => bail!("unknown xtask command: {other:?}"),
    }
}
