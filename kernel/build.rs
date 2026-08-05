//! Assembles the flat user binaries the kernel embeds.
//!
//! The alternative is committing a `.bin` blob next to its `.s`, which drifts
//! the first time someone edits one and not the other. Assembling here means
//! the source is the only source of truth, at the cost of requiring clang and
//! LLD -- which this project already requires everywhere else (see
//! `.cargo/config.toml`), so it adds no new dependency.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR unset"));
    let src = PathBuf::from("user/init.s");
    println!("cargo:rerun-if-changed={}", src.display());

    let obj = out.join("init.o");
    run(
        Command::new("clang")
            .args(["-target", "x86_64-unknown-none", "-nostdlib", "-c", "-o"])
            .arg(&obj)
            .arg(&src),
        "clang",
    );

    // `--oformat=binary` so the result is raw machine code at a known base,
    // not an ELF. The ELF loader is a separate concern; this is the program
    // that proves the ring-3 transition works without one.
    let bin = out.join("init.bin");
    run(
        Command::new("ld.lld")
            .args(["--oformat=binary", "-Ttext=0x400000", "-e", "_start", "-o"])
            .arg(&bin)
            .arg(&obj),
        "ld.lld",
    );
}

fn run(cmd: &mut Command, what: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to run {what}: {e}; is it installed?"));
    assert!(status.success(), "{what} failed while assembling the init binary");
}
