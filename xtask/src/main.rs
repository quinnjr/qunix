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

/// Cargo arguments for `xtask bench`, with any extra flags appended.
///
/// Split out from the spawn so the crate/feature selection is testable. Only
/// host-buildable crates appear: criterion needs `std`, and the kernel is
/// `no_std` running in QEMU, so it cannot be linked against at all.
fn bench_args(extra: &[String]) -> Vec<String> {
    let mut args: Vec<String> = [
        "bench",
        "--target",
        "x86_64-unknown-linux-musl",
        "-p",
        "qunix-sync",
        "-p",
        "qunix-mm",
        "-p",
        "qunix-hal-x86_64",
        "--features",
        "qunix-sync/std,qunix-mm/std,qunix-hal-x86_64/std",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    args.extend(extra.iter().cloned());
    args
}

/// Cargo arguments for `xtask fuzz`, given a target name and a time budget.
///
/// Split from the spawn for the same reason as [`bench_args`]: the selection is
/// worth pinning, the `Command` is not testable. `-max_total_time` is passed
/// rather than letting libFuzzer run forever, so this is usable in CI and in a
/// pre-PR check without needing to be interrupted by hand.
fn fuzz_args(target: &str, seconds: u32, extra: &[String]) -> Vec<String> {
    let mut args: Vec<String> = ["fuzz", "run", target]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    // Pinned, not defaulted. cargo-fuzz targets the triple *it* was built for,
    // so a source-built copy picks gnu while the prebuilt binary CI downloads
    // is musl-static -- and a sanitizer cannot be linked into a static libc
    // ("sanitizer is incompatible with statically linked libc"). Naming the
    // triple makes the build identical however cargo-fuzz was installed.
    args.push("--target".to_string());
    args.push("x86_64-unknown-linux-gnu".to_string());
    // No `--locked` here: `cargo fuzz run` has its own CLI and rejects it
    // ("unexpected argument"). The committed `fuzz/Cargo.lock` is enforced by
    // the `lockfile` CI step instead, which runs `cargo metadata --locked`
    // against the fuzz manifest.
    args.push("--".to_string());
    args.push(format!("-max_total_time={seconds}"));
    // libFuzzer's default 2 GiB ceiling counts the sanitizer's shadow memory,
    // which the buddy target's 16 MiB arena plus ASan redzones can approach.
    args.push("-rss_limit_mb=4096".to_string());
    args.extend(extra.iter().cloned());
    args
}

/// Fuzz targets run by a bare `xtask fuzz`.
const FUZZ_TARGETS: &[&str] = &["buddy", "slab"];

/// Resolves `xtask fuzz` arguments into (targets, seconds, libFuzzer passthrough).
///
/// Extracted from the dispatch arm because these are decisions, not a spawn:
/// the "command dispatch is uncoverable" exemption does not cover them, and
/// each of the three rejections below is a bug that shipped silently before.
fn fuzz_selection(args: &[String]) -> Result<(Vec<&'static str>, u32, Vec<String>)> {
    // A malformed budget is refused rather than defaulted. Silently running 60s
    // when the user asked for 600 produces a run they will believe was ten
    // minutes -- CONTRIBUTING tells contributors to do exactly that before a PR.
    let seconds = match args.iter().find_map(|a| a.strip_prefix("--seconds=")) {
        Some(value) => {
            let parsed: u32 = value
                .parse()
                .with_context(|| format!("--seconds={value} is not a whole number of seconds"))?;
            // libFuzzer reads `-max_total_time=0` as *no limit*, so a zero
            // budget is the one value that produces the hang every other part
            // of this function exists to prevent.
            if parsed == 0 {
                bail!("--seconds=0 means unlimited to libFuzzer; pass a positive budget");
            }
            parsed
        }
        None => 60,
    };

    let named: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let targets: Vec<&'static str> = match named.first() {
        Some(name) => {
            // Checked here so a typo reads as a typo, rather than surfacing as
            // "fuzz target `budy` failed" after a full sanitizer build.
            let found = FUZZ_TARGETS.iter().find(|t| *t == &name.as_str());
            match found {
                Some(t) => vec![*t],
                None => bail!(
                    "unknown fuzz target `{name}`; known targets: {}",
                    FUZZ_TARGETS.join(", ")
                ),
            }
        }
        None => FUZZ_TARGETS.to_vec(),
    };

    // Everything else goes to libFuzzer, matching what `bench` does for
    // criterion. Without this the `extra` parameter is reachable only from its
    // own unit test, which is not a test of anything.
    let extra: Vec<String> = args
        .iter()
        .filter(|a| !a.starts_with("--seconds=") && Some(*a) != named.first().copied())
        .cloned()
        .collect();
    Ok((targets, seconds, extra))
}

mod attest;
mod coverage;
mod image;
mod licensing;
mod qemu;
mod userland;

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
            // Twice, and the second run is not redundant.
            //
            // `sched-invariants` makes `publish_handoff` assert, under the
            // scheduler lock, that it is not queueing a parked thread. That
            // check is worth having -- it catches the violation at the push
            // rather than by sampling later -- but it costs a lock acquisition
            // on every context switch, which gives the test kernel a
            // serialisation point the shipped one does not have. Every
            // concurrency test would then be measuring a scheduler that is not
            // the one that ships, and a race the real kernel loses and the test
            // kernel wins would be invisible by construction.
            //
            // So: one boot with the invariant enforced, one with honest timing.
            for invariants in [true, false] {
                let mut cmd = Command::new(env!("CARGO"));
                cmd.current_dir(&root);
                cmd.args(["test", "--package", "qunix-kernel"]);
                if invariants {
                    cmd.args(["--features", "sched-invariants"]);
                }
                cmd.args(BUILD_STD);
                // Without this, cargo forces `panic=unwind` for test units,
                // which makes build-std compile `core` a second time and
                // collide with the panic=abort copy (E0152: duplicate lang
                // item).
                cmd.arg("-Zpanic-abort-tests");
                // The profiles differ behaviourally (`lto = "thin"`, and
                // overflow checks only in debug), so `--release` has to reach
                // every child.
                if release {
                    cmd.arg("--release");
                }
                if !cmd.status()?.success() {
                    bail!(
                        "kernel tests failed ({})",
                        if invariants {
                            "with scheduler invariants enforced"
                        } else {
                            "with unmodified scheduler timing"
                        }
                    );
                }
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
                "qunix-sched",
                "-p",
                "qunix-elf",
                "-p",
                "xtask",
                "--features",
                "qunix-sync/std,qunix-mm/std,qunix-hal-x86_64/std,qunix-sched/std,qunix-elf/std",
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
        Some("bench") => {
            let mut cmd = Command::new(env!("CARGO"));
            cmd.current_dir(&root);
            cmd.args(bench_args(&args[1..]));
            if !cmd.status()?.success() {
                bail!("benchmarks failed");
            }
            Ok(())
        }
        Some("fuzz") => {
            let (selected, seconds, extra) = fuzz_selection(&args[1..])?;
            let mut failed: Vec<&str> = Vec::new();
            for target in selected {
                println!("fuzz: {target} for {seconds}s");
                let mut cmd = Command::new(env!("CARGO"));
                cmd.current_dir(&root);
                cmd.args(fuzz_args(target, seconds, &extra));
                // The hint belongs on the failure that actually happens. The
                // spawn is of cargo itself and essentially cannot fail; a
                // missing cargo-fuzz makes cargo exit non-zero with "no such
                // command: fuzz", which is this branch, not the spawn error.
                if !cmd.status().context("failed to spawn cargo")?.success() {
                    failed.push(target);
                }
            }
            if !failed.is_empty() {
                bail!(
                    "fuzz target(s) failed: {} \
                     (if this was `no such command: fuzz`, run `cargo install cargo-fuzz`)",
                    failed.join(", ")
                );
            }
            Ok(())
        }
        Some("attest") => attest::check(&root, args.get(1).map(String::as_str)),
        Some("coverage") => coverage::check(&root, args.iter().any(|a| a == "--update")),
        other => bail!("unknown xtask command: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{FUZZ_TARGETS, bench_args, fuzz_args, fuzz_selection, workspace_root};

    #[test]
    fn bench_selects_only_host_buildable_crates() {
        let args = bench_args(&[]);
        for name in ["qunix-sync", "qunix-mm", "qunix-hal-x86_64"] {
            assert!(args.iter().any(|a| a == name), "missing {name}: {args:?}");
        }
        // criterion needs std; the kernel is no_std running in QEMU.
        assert!(!args.iter().any(|a| a == "qunix-kernel"));
    }

    #[test]
    fn bench_enables_the_std_feature_of_every_selected_crate() {
        let args = bench_args(&[]);
        let features = args.iter().find(|a| a.contains("/std")).expect("no --features value");
        for name in ["qunix-sync", "qunix-mm", "qunix-hal-x86_64"] {
            assert!(features.contains(&format!("{name}/std")), "missing {name}/std in {features}");
        }
    }

    #[test]
    fn bench_forwards_extra_arguments_after_the_separator() {
        let args = bench_args(&["--".to_string(), "alloc_free".to_string()]);
        // The property that matters is placement, not presence: before `--`
        // cargo consumes them, after it criterion does. Asserting only that the
        // last two elements equal the input is true of any `extend`.
        let sep = args.iter().position(|a| a == "--").expect("no -- separator");
        assert!(args[sep + 1..].iter().any(|a| a == "alloc_free"));
    }

    #[test]
    fn bench_pins_the_host_triple_and_locks_nothing_else() {
        let args = bench_args(&[]);
        assert!(args.windows(2).any(|w| w[0] == "--target"));
    }

    #[test]
    fn fuzz_selection_defaults_to_every_listed_target() {
        let (targets, seconds, extra) = fuzz_selection(&[]).unwrap();
        assert_eq!(targets, FUZZ_TARGETS);
        assert_eq!(seconds, 60);
        assert!(extra.is_empty());
    }

    #[test]
    fn fuzz_selection_rejects_an_unparsable_budget() {
        // Must error, not silently fall back to 60 -- a typo in `--seconds=600`
        // would otherwise produce a run the user believes was ten minutes.
        let err = fuzz_selection(&["--seconds=abc".to_string()]).unwrap_err();
        assert!(err.to_string().contains("--seconds=abc"), "unhelpful error: {err}");
    }

    #[test]
    fn fuzz_selection_rejects_a_zero_budget() {
        // `-max_total_time=0` is libFuzzer's *unlimited* sentinel.
        let err = fuzz_selection(&["--seconds=0".to_string()]).unwrap_err();
        assert!(err.to_string().contains("unlimited"), "unhelpful error: {err}");
    }

    #[test]
    fn fuzz_selection_rejects_an_unknown_target() {
        let err = fuzz_selection(&["budy".to_string()]).unwrap_err();
        assert!(err.to_string().contains("unknown fuzz target"), "unhelpful error: {err}");
    }

    #[test]
    fn fuzz_selection_forwards_libfuzzer_flags_from_the_command_line() {
        // Regression guard: `extra` was accepted by `fuzz_args` but the dispatch
        // arm hard-coded `&[]`, so no user input could ever reach it.
        let (targets, _, extra) =
            fuzz_selection(&["buddy".to_string(), "-runs=1".to_string()]).unwrap();
        assert_eq!(targets, vec!["buddy"]);
        assert!(extra.iter().any(|a| a == "-runs=1"), "libFuzzer flag was dropped: {extra:?}");
    }

    #[test]
    fn fuzz_bounds_the_run_and_names_the_target() {
        let args = fuzz_args("buddy", 90, &[]);
        assert_eq!(args[..3], ["fuzz", "run", "buddy"]);
        // A sanitizer cannot link against a static libc, so the triple must be
        // pinned rather than inherited from how cargo-fuzz was installed.
        assert!(
            args.windows(2).any(|w| w[0] == "--target" && w[1] == "x86_64-unknown-linux-gnu"),
            "fuzz target triple not pinned: {args:?}"
        );
        // Unbounded, libFuzzer never returns, which would hang CI.
        assert!(args.iter().any(|a| a == "-max_total_time=90"), "no time bound: {args:?}");
        assert!(args.iter().any(|a| a.starts_with("-rss_limit_mb=")));
        // `cargo fuzz run` rejects cargo flags it does not define, so nothing
        // may be added here that has not been checked against its CLI.
        let sep = args.iter().position(|a| a == "--").expect("no -- separator");
        assert!(!args[..sep].iter().any(|a| a == "--locked"), "cargo fuzz rejects --locked");
    }

    #[test]
    fn fuzz_passes_libfuzzer_flags_after_the_separator() {
        let args = fuzz_args("slab", 10, &["-runs=1".to_string()]);
        let sep = args.iter().position(|a| a == "--").expect("no -- separator");
        // Anything before `--` is consumed by cargo-fuzz, never by libFuzzer.
        assert!(args[sep + 1..].iter().any(|a| a == "-runs=1"));
    }

    #[test]
    fn fuzz_targets_sources_and_manifest_all_agree() {
        let root = workspace_root();
        let manifest = std::fs::read_to_string(root.join("fuzz/Cargo.toml")).unwrap();
        for target in FUZZ_TARGETS {
            let path = root.join("fuzz/fuzz_targets").join(format!("{target}.rs"));
            assert!(path.exists(), "{target} is listed but {} is missing", path.display());
            // cargo-fuzz dispatches on the `[[bin]]` name, so a source file
            // alone is not enough -- the mismatch would only appear in CI.
            assert!(
                manifest.contains(&format!("name = \"{target}\"")),
                "{target} is listed but has no [[bin]] in fuzz/Cargo.toml"
            );
        }
        // The direction that degrades silently: a target added on disk but not
        // listed here is never run, and CI stays green.
        for entry in std::fs::read_dir(root.join("fuzz/fuzz_targets")).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().replace(".rs", "");
            assert!(
                FUZZ_TARGETS.contains(&name.as_str()),
                "fuzz_targets/{name}.rs exists but is not in FUZZ_TARGETS, so nothing runs it"
            );
        }
    }
}
