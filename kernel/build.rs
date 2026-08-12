//! Points the linker at `linker.ld`.
//!
//! Not in `.cargo/config.toml` alongside the other kernel link flags, because
//! `-T` needs a path and rustflags there are resolved against the directory
//! cargo was invoked from -- so a relative path breaks the moment anything runs
//! cargo from a subdirectory, and an absolute one cannot be written down. This
//! is the only place that knows where the crate is.

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let script = format!("{dir}/linker.ld");
    // `rustc-link-arg` rather than `-bins`: the in-QEMU test suite is a *test*
    // binary and it boots on the same hardware, so it needs the same layout.
    println!("cargo::rustc-link-arg=-T{script}");
    println!("cargo::rerun-if-changed=linker.ld");
    println!("cargo::rerun-if-changed=build.rs");
}
