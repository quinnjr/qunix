# qunix M0 — Boot and Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Boot a Rust `no_std` kernel under QEMU via Limine, print to serial, install GDT/IDT/APIC, and bring up a buddy frame allocator plus a slab heap so `Box` and `Vec` work in kernel space.

**Architecture:** A Cargo workspace with a thin `kernel` binary crate over focused library crates (`qunix-sync`, `qunix-hal-x86_64`, `qunix-mm`). Pure-logic crates (locking, allocators) build `no_std` but expose a `std` feature so they are unit-tested on the host; hardware-touching code is tested inside QEMU through a custom test runner that signals results via the `isa-debug-exit` device. An `xtask` crate owns all build, image-assembly, and QEMU orchestration so no shell scripts are needed.

**Tech Stack:** Rust (pinned nightly, edition 2024), Limine boot protocol via the `limine` crate 0.6.5, the `x86_64` crate 0.15 for descriptor-table and paging structures, QEMU with OVMF, `xorriso` for ISO assembly.

## Global Constraints

- **MSRV:** `rust-version = "1.97"` in every crate manifest. Toolchain is a pinned nightly, because `-Z build-std` is required for a custom bare-metal target and is nightly-only.
- **Edition:** `2024` for all crates. Edition 2024 requires `#[unsafe(no_mangle)]` and `#[unsafe(link_section = "...")]` — the bare forms will not compile.
- **Target:** `targets/x86_64-qunix-kernel.json`, built with `-Z build-std=core,compiler_builtins,alloc` and `-Z build-std-features=compiler-builtins-mem`.
- **Kernel crates are `no_std`.** `xtask` is the only host-targeted crate.
- **No floating point in kernel code.** The target disables MMX/SSE and uses soft-float; using `f32`/`f64` in kernel crates is a bug.
- **Licence:** all crates in this milestone are `MIT OR Apache-2.0` (permissive core, per spec §9). No GPL-2.0 crates exist yet.
- **Pinned dependency versions:** `limine = "=0.6.5"`, `x86_64 = "0.15"`.
- **Every commit must leave `cargo xtask test` passing.**

---

## Execution Deviations

Recorded during execution. The code in Task 1's steps below is superseded by
these; read the real files for current truth.

1. **Toolchain pinned to `nightly-2026-08-02`**, not `-08-01`. That is the
   nightly actually published and installed.
2. **`-Z build-std` moved out of `.cargo/config.toml` into `xtask`.** It is a
   *global* unstable flag, so in the config file it also applied to the
   host-targeted `xtask`, which rebuilt `core` from source alongside the
   precompiled `std` and failed with `E0152: duplicate lang item`.
3. **`-Zjson-target-spec` is now required** for JSON target specs.
4. **The target-spec JSON schema changed.** `target-pointer-width` is a number,
   not a string; `rustc-abi` is `"softfloat"`, not `"x86-softfloat"`; and
   `os`, `vendor`, `executables`, `target-c-int-width` are gone. The spec is now
   derived from `rustc -Zunstable-options --target x86_64-unknown-none
   --print target-spec-json`, changing only PIE and relocation model.
5. **No GNU dependencies** (user request):
   - Host crates target `x86_64-unknown-linux-musl`, not `-gnu`.
   - Inspection uses `llvm-readobj` / `llvm-addr2line`, not GNU binutils.
   - **`kernel/linker.ld` and `kernel/build.rs` do not exist.** The GNU ld
     script is replaced by LLD flags in `.cargo/config.toml`:
     `--image-base=0xffffffff80000000` and `--entry=kmain`.
     Consequence: there is no `.requests` PHDR and no
     `.requests_start_marker` / `.requests_end_marker` placement control.
     Limine's markers are an optional scan optimisation, so Task 3 must omit
     them and let Limine scan the whole image for request magic.
6. **`crates/*` is not in the workspace `members` until Task 2**, because a
   glob matching a non-existent directory is a hard error.
7. **`xorriso` is not used at all.** It was not installed, and turned out to be
   unnecessary: the ESP is assembled as a plain directory and handed to QEMU via
   VVFAT (`-drive format=raw,file=fat:rw:<dir>`) under OVMF. That also removed
   the `make` step and the BIOS install path, since only `BOOTX64.EFI` is
   needed. **Consequence: qunix is UEFI-only.** The exit criterion "boots under
   both BIOS and UEFI" is reduced to UEFI, and `xtask` has no `--bios` flag.
8. **Limine is pinned to `v11.x-binary`, not `v9.x`.** The `limine` crate 0.6.5
   requests base revision 6 (`BaseRevision::MAX_SUPPORTED`); v9.x predates that,
   so the handshake failed and `is_supported()` returned false.
9. **`Request::new()` needs a turbofish or the request aliases.** Inference
   cannot choose among the per-response `new()` impls. The code uses
   `HhdmRequest` / `MemmapRequest`.
10. **The limine 0.6.5 memory-map API differs from the plan's guess.** `offset`
    is a *field* on `HhdmRespData`, not a method; entries expose `type_`
    compared against `MEMMAP_USABLE`, and there is no `EntryType` enum.
11. **`-Zpanic-abort-tests` is required.** Cargo forces `panic=unwind` for test
    units, which made build-std compile a second `core` and collide with the
    `panic=abort` copy (`E0152: duplicate lang item`).
12. **`-no-shutdown` had to be dropped from the QEMU invocation.** It keeps QEMU
    alive after `isa-debug-exit` fires, so the runner never observed an exit code.
13. **The stack-overflow double-fault check does not work (Task 6, Step 5).**
    Limine's stack has no guard page below it, so deep recursion silently
    scribbles through usable memory and hangs rather than trapping. The same
    escalation path is instead verified deterministically by pointing RSP at
    unmapped memory and pushing. A guard-page overflow test becomes possible in
    M1, once the kernel allocates its own thread stacks.
14. **The LAPIC MMIO page is not in the HHDM.** Limine's direct map covers RAM
    only, so `apic::init` page-faulted. The kernel now maps `0xFEE00000`
    explicitly and uncacheable, which added `PageFlags::NO_CACHE`.
15. **Frame pointers are enabled via `-C force-frame-pointers=yes` in
    `.cargo/config.toml`, not a profile key.** Cargo has no such profile option,
    so the plan's `[profile.dev] force-frame-pointers` would have been ignored
    silently and every backtrace would have been empty.
16. **The backtrace walker needs two address thresholds, not one.** Kernel
    stacks live in HHDM-mapped RAM near `0xffff8000_00000000`, far below the
    kernel text base at -2 GiB, so validating RBP against the text base rejected
    every frame. RBP is checked against the higher-half boundary; only return
    addresses are checked against the text base.
17. **Task 1 Step 6's linker script was skipped, and that was wrong.** Two LLD
    flags (`--image-base`, `--entry`) stood in for it, which satisfied the only
    requirement anyone checked — the kernel starts in the higher half — while
    leaving every section an *orphan*, placed by name in whatever order that
    produced. The Limine request markers were therefore omitted, because orphan
    placement puts `.requests_end_marker` *before* `.requests_start_marker` with
    `.requests` outside the pair entirely; the bootloader fell back to scanning
    the whole image for request magic. That fallback is not a guarantee the
    protocol makes, and it stopped working during M2 T4: a release build failed
    `kmain`'s base-revision assertion while the same source booted in debug, and
    a bisect named a commit of renames and doc comments — the signature of a
    layout dependency. `kernel/linker.ld` now exists as this plan specified,
    passed by `kernel/build.rs`, and the flags it replaced are gone from
    `.cargo/config.toml`. It also fixed a permission bug the flags had hidden:
    the requests must be writable (the bootloader writes each response pointer),
    so with a single orphan-placed data segment the kernel's entire `.rodata`
    was writable. There are now four segments — RX, RW requests, R rodata, RW
    data — and `readelf -l` shows the boundaries.

## File Structure

| Path | Responsibility |
| --- | --- |
| `Cargo.toml` | Virtual workspace manifest; shared `[workspace.package]` and `[workspace.dependencies]` |
| `rust-toolchain.toml` | Pinned nightly + `rust-src`, `llvm-tools` components |
| `.cargo/config.toml` | build-std flags, default target, custom-target runner, `xtask` alias |
| `targets/x86_64-qunix-kernel.json` | Custom target specification |
| `kernel/linker.ld` | Higher-half link script satisfying the Limine protocol |
| `kernel/src/main.rs` | Entry point and init sequence only |
| `kernel/src/boot.rs` | Limine requests; extracts HHDM offset and memory map |
| `kernel/src/panic.rs` | Panic handler and frame-pointer backtrace |
| `kernel/src/testing.rs` | Custom test runner, `isa-debug-exit` codes |
| `crates/qunix-sync/src/lib.rs` | `SpinLock`, `IrqSpinLock` |
| `crates/qunix-hal-x86_64/src/port.rs` | Port I/O primitives |
| `crates/qunix-hal-x86_64/src/serial.rs` | 16550 UART driver |
| `crates/qunix-hal-x86_64/src/gdt.rs` | GDT and TSS |
| `crates/qunix-hal-x86_64/src/idt.rs` | IDT and exception handlers |
| `crates/qunix-hal-x86_64/src/apic.rs` | Local APIC and timer |
| `crates/qunix-hal-x86_64/src/paging.rs` | Page-table access over the HHDM |
| `crates/qunix-mm/src/buddy.rs` | Buddy physical frame allocator |
| `crates/qunix-mm/src/slab.rs` | Size-class slab heap implementing `GlobalAlloc` |
| `xtask/src/main.rs` | Command dispatch: `build`, `run`, `test`, `runner` |
| `xtask/src/image.rs` | Limine acquisition and ISO assembly |
| `xtask/src/qemu.rs` | QEMU invocation, OVMF discovery, exit-code mapping |

---

### Task 1: Workspace, Toolchain, and Build Plumbing

**Files:**
- Modify: `Cargo.toml` (convert package to virtual workspace)
- Create: `rust-toolchain.toml`, `.cargo/config.toml`, `targets/x86_64-qunix-kernel.json`
- Create: `kernel/Cargo.toml`, `kernel/src/main.rs`, `kernel/linker.ld`
- Create: `xtask/Cargo.toml`, `xtask/src/main.rs`
- Delete: `src/main.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: a `cargo xtask` alias, and an ELF at `target/x86_64-qunix-kernel/debug/qunix-kernel`.

- [ ] **Step 1: Replace the root manifest with a virtual workspace**

```toml
# Cargo.toml
[workspace]
resolver = "3"
members = ["kernel", "xtask", "crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.97"
license = "MIT OR Apache-2.0"

[workspace.dependencies]
limine = "=0.6.5"
x86_64 = "0.15"
qunix-sync = { path = "crates/qunix-sync" }
qunix-mm = { path = "crates/qunix-mm" }
qunix-hal-x86_64 = { path = "crates/qunix-hal-x86_64" }

[profile.dev]
panic = "abort"

[profile.release]
panic = "abort"
lto = true
```

- [ ] **Step 2: Remove the old binary source**

```bash
rm -rf src
```

- [ ] **Step 3: Pin the toolchain**

```toml
# rust-toolchain.toml
[toolchain]
channel = "nightly-2026-08-01"
components = ["rust-src", "llvm-tools", "rustfmt", "clippy"]
targets = ["x86_64-unknown-linux-musl"]
```

- [ ] **Step 4: Write the custom target specification**

```json
{
  "llvm-target": "x86_64-unknown-none",
  "data-layout": "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128",
  "arch": "x86_64",
  "target-endian": "little",
  "target-pointer-width": "64",
  "target-c-int-width": "32",
  "os": "none",
  "vendor": "unknown",
  "executables": true,
  "linker-flavor": "gnu-lld",
  "linker": "rust-lld",
  "panic-strategy": "abort",
  "disable-redzone": true,
  "features": "-mmx,-sse,+soft-float",
  "rustc-abi": "x86-softfloat",
  "code-model": "kernel",
  "relocation-model": "static",
  "static-position-independent-executables": false
}
```

`rustc-abi: x86-softfloat` is mandatory whenever SSE is disabled on x86_64; without it the compiler rejects the target.

- [ ] **Step 5: Write the cargo configuration**

```toml
# .cargo/config.toml
[build]
target = "targets/x86_64-qunix-kernel.json"

[unstable]
build-std = ["core", "compiler_builtins", "alloc"]
build-std-features = ["compiler-builtins-mem"]

[target.x86_64-qunix-kernel]
runner = ["cargo", "run", "--quiet", "--package", "xtask",
          "--target", "x86_64-unknown-linux-musl", "--", "runner"]

[alias]
xtask = "run --quiet --package xtask --target x86_64-unknown-linux-musl --"
```

The explicit `--target x86_64-unknown-linux-musl` is required: `[build] target` would otherwise cross-compile `xtask` itself to the bare-metal target.

- [ ] **Step 6: Write the linker script**

```ld
/* kernel/linker.ld */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(kmain)

PHDRS
{
    requests PT_LOAD;
    text     PT_LOAD;
    rodata   PT_LOAD;
    data     PT_LOAD;
}

SECTIONS
{
    . = 0xffffffff80000000;

    .requests : {
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))
    } :requests

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .text : { *(.text .text.*) } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .rodata : { *(.rodata .rodata.*) } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .data : { *(.data .data.*) } :data
    .bss  : { *(.bss .bss.*) *(COMMON) } :data
}
```

- [ ] **Step 7: Create the kernel crate**

```toml
# kernel/Cargo.toml
[package]
name = "qunix-kernel"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[[bin]]
name = "qunix-kernel"
path = "src/main.rs"
test = false

[dependencies]
```

```rust
// kernel/src/main.rs
#![no_std]
#![no_main]

use core::panic::PanicInfo;

#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    halt_forever();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    halt_forever();
}

fn halt_forever() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}
```

Add the link-script flag via a `build.rs` so it applies only to this crate:

```rust
// kernel/build.rs
fn main() {
    println!("cargo::rustc-link-arg=-T{}/linker.ld", env!("CARGO_MANIFEST_DIR"));
    println!("cargo::rerun-if-changed=linker.ld");
}
```

- [ ] **Step 8: Create the xtask crate with a `build` command**

```toml
# xtask/Cargo.toml
[package]
name = "xtask"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
anyhow = "1"
```

```rust
// xtask/src/main.rs
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn build_kernel(release: bool) -> Result<PathBuf> {
    let mut cmd = Command::new(env!("CARGO"));
    cmd.current_dir(workspace_root());
    cmd.args(["build", "--package", "qunix-kernel"]);
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

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("build") => {
            let elf = build_kernel(args.iter().any(|a| a == "--release"))?;
            println!("kernel: {}", elf.display());
            Ok(())
        }
        other => bail!("unknown xtask command: {other:?}"),
    }
}
```

- [ ] **Step 9: Verify the kernel builds and is a valid ELF**

Run:
```bash
cargo xtask build
"$(rustc --print sysroot)"/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-readobj \
  --elf-output-style=GNU -h target/x86_64-qunix-kernel/debug/qunix-kernel
```
Expected: `Class: ELF64`, `Machine: Advanced Micro Devices X86-64`, and an entry point of `0xffffffff80000000` or higher.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml rust-toolchain.toml .cargo targets kernel xtask
git rm -r --cached src 2>/dev/null || true
git commit -m "build: bare-metal workspace, custom target, and xtask"
```

---

### Task 2: Spinlocks (`qunix-sync`)

Pure logic, so this is tested on the host. Written before serial because the console needs a lock.

**Files:**
- Create: `crates/qunix-sync/Cargo.toml`, `crates/qunix-sync/src/lib.rs`
- Test: `crates/qunix-sync/src/lib.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `SpinLock<T>::new(T) -> SpinLock<T>`
  - `SpinLock<T>::lock(&self) -> SpinLockGuard<'_, T>` (derefs to `T`, `DerefMut`)
  - `SpinLock<T>::try_lock(&self) -> Option<SpinLockGuard<'_, T>>`
  - `IrqSpinLock<T>` with the same surface; disables interrupts for the guard's lifetime.

- [ ] **Step 1: Create the crate manifest**

```toml
# crates/qunix-sync/Cargo.toml
[package]
name = "qunix-sync"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[features]
default = []
std = []
```

- [ ] **Step 2: Write the failing tests**

```rust
// crates/qunix-sync/src/lib.rs  (append at end of file)
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_grants_mutable_access() {
        let lock = SpinLock::new(41);
        *lock.lock() += 1;
        assert_eq!(*lock.lock(), 42);
    }

    #[test]
    fn try_lock_fails_while_held() {
        let lock = SpinLock::new(0);
        let _guard = lock.lock();
        assert!(lock.try_lock().is_none());
    }

    #[test]
    fn try_lock_succeeds_after_drop() {
        let lock = SpinLock::new(0);
        drop(lock.lock());
        assert!(lock.try_lock().is_some());
    }

    #[test]
    fn contended_across_threads_never_loses_increments() {
        use std::sync::Arc;
        let lock = Arc::new(SpinLock::new(0usize));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let lock = Arc::clone(&lock);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        *lock.lock() += 1;
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*lock.lock(), 8000);
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p qunix-sync --features std --target x86_64-unknown-linux-musl`
Expected: FAIL — `cannot find type SpinLock in this scope`.

- [ ] **Step 4: Implement the lock**

```rust
// crates/qunix-sync/src/lib.rs  (top of file)
#![cfg_attr(not(test), no_std)]

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), data: UnsafeCell::new(value) }
    }
}

impl<T: ?Sized> SpinLock<T> {
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
    }

    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SpinLockGuard { lock: self })
    }
}

pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
}

impl<T: ?Sized> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p qunix-sync --features std --target x86_64-unknown-linux-musl`
Expected: PASS, 4 tests.

- [ ] **Step 6: Add the interrupt-safe variant**

```rust
// crates/qunix-sync/src/lib.rs  (append before the tests module)
/// Hook the arch layer installs so `IrqSpinLock` can mask interrupts.
/// Returns the previous interrupt-enable state.
pub trait IrqControl {
    fn disable_and_save() -> bool;
    fn restore(was_enabled: bool);
}

pub struct IrqSpinLock<T: ?Sized, I: IrqControl> {
    inner: SpinLock<T>,
    _irq: core::marker::PhantomData<I>,
}

unsafe impl<T: ?Sized + Send, I: IrqControl> Send for IrqSpinLock<T, I> {}
unsafe impl<T: ?Sized + Send, I: IrqControl> Sync for IrqSpinLock<T, I> {}

impl<T, I: IrqControl> IrqSpinLock<T, I> {
    pub const fn new(value: T) -> Self {
        Self { inner: SpinLock::new(value), _irq: core::marker::PhantomData }
    }

    pub fn lock(&self) -> IrqSpinLockGuard<'_, T, I> {
        let was_enabled = I::disable_and_save();
        let guard = self.inner.lock();
        IrqSpinLockGuard { guard: Some(guard), was_enabled, _irq: core::marker::PhantomData }
    }
}

pub struct IrqSpinLockGuard<'a, T: ?Sized, I: IrqControl> {
    guard: Option<SpinLockGuard<'a, T>>,
    was_enabled: bool,
    _irq: core::marker::PhantomData<I>,
}

impl<T: ?Sized, I: IrqControl> Deref for IrqSpinLockGuard<'_, T, I> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_ref().unwrap()
    }
}

impl<T: ?Sized, I: IrqControl> DerefMut for IrqSpinLockGuard<'_, T, I> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().unwrap()
    }
}

impl<T: ?Sized, I: IrqControl> Drop for IrqSpinLockGuard<'_, T, I> {
    fn drop(&mut self) {
        // Release the spinlock before restoring interrupts, so an interrupt
        // handler that takes the same lock cannot deadlock against us.
        self.guard.take();
        I::restore(self.was_enabled);
    }
}
```

- [ ] **Step 7: Test the interrupt-safe variant**

```rust
// crates/qunix-sync/src/lib.rs  (inside mod tests)
    struct FakeIrq;
    static IRQ_DEPTH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    impl IrqControl for FakeIrq {
        fn disable_and_save() -> bool {
            IRQ_DEPTH.fetch_add(1, Ordering::SeqCst);
            true
        }
        fn restore(_was_enabled: bool) {
            IRQ_DEPTH.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn irq_lock_restores_interrupt_state_on_drop() {
        let lock: IrqSpinLock<u32, FakeIrq> = IrqSpinLock::new(7);
        assert_eq!(IRQ_DEPTH.load(Ordering::SeqCst), 0);
        {
            let guard = lock.lock();
            assert_eq!(*guard, 7);
            assert_eq!(IRQ_DEPTH.load(Ordering::SeqCst), 1);
        }
        assert_eq!(IRQ_DEPTH.load(Ordering::SeqCst), 0);
    }
```

Run: `cargo test -p qunix-sync --features std --target x86_64-unknown-linux-musl`
Expected: PASS, 5 tests.

- [ ] **Step 8: Commit**

```bash
git add crates/qunix-sync
git commit -m "feat(sync): spinlock and interrupt-safe spinlock"
```

---

### Task 3: Limine Boot, Serial Output, and QEMU Run

**Files:**
- Create: `crates/qunix-hal-x86_64/Cargo.toml`, `src/lib.rs`, `src/port.rs`, `src/serial.rs`
- Create: `kernel/src/boot.rs`, `limine.conf`
- Create: `xtask/src/image.rs`, `xtask/src/qemu.rs`
- Modify: `kernel/src/main.rs`, `kernel/Cargo.toml`, `xtask/src/main.rs`, `xtask/Cargo.toml`

**Interfaces:**
- Consumes: `qunix_sync::{SpinLock, IrqControl}`.
- Produces:
  - `qunix_hal_x86_64::port::{inb, outb, inl, outl}`
  - `qunix_hal_x86_64::serial::init()` and the `qunix_hal_x86_64::{print, println}` macros
  - `qunix_hal_x86_64::Irq` implementing `qunix_sync::IrqControl`
  - `xtask` commands `run` and `runner <elf-path>`

- [ ] **Step 1: Create the HAL crate with port I/O**

```toml
# crates/qunix-hal-x86_64/Cargo.toml
[package]
name = "qunix-hal-x86_64"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
qunix-sync.workspace = true
x86_64.workspace = true
```

```rust
// crates/qunix-hal-x86_64/src/port.rs
use core::arch::asm;

/// # Safety
/// Port I/O can have arbitrary side effects on hardware.
pub unsafe fn outb(port: u16, value: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags)) };
}

/// # Safety
/// Port I/O can have arbitrary side effects on hardware.
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags)) };
    value
}

/// # Safety
/// Port I/O can have arbitrary side effects on hardware.
pub unsafe fn outl(port: u16, value: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags)) };
}

/// # Safety
/// Port I/O can have arbitrary side effects on hardware.
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe { asm!("in eax, dx", out("eax") value, in("dx") port, options(nomem, nostack, preserves_flags)) };
    value
}
```

- [ ] **Step 2: Implement the 16550 UART driver**

```rust
// crates/qunix-hal-x86_64/src/serial.rs
use crate::port::{inb, outb};
use core::fmt::{self, Write};
use qunix_sync::SpinLock;

const COM1: u16 = 0x3F8;

pub struct Uart {
    base: u16,
}

impl Uart {
    const fn new(base: u16) -> Self {
        Self { base }
    }

    /// # Safety
    /// Must only be called once per physical UART.
    unsafe fn init(&mut self) {
        unsafe {
            outb(self.base + 1, 0x00); // disable interrupts
            outb(self.base + 3, 0x80); // enable DLAB
            outb(self.base + 0, 0x03); // divisor lo: 38400 baud
            outb(self.base + 1, 0x00); // divisor hi
            outb(self.base + 3, 0x03); // 8N1, DLAB off
            outb(self.base + 2, 0xC7); // enable + clear FIFO, 14-byte threshold
            outb(self.base + 4, 0x0B); // DTR, RTS, OUT2
        }
    }

    fn write_byte(&mut self, byte: u8) {
        // Bit 5 of the line-status register means "transmit holding empty".
        while unsafe { inb(self.base + 5) } & 0x20 == 0 {
            core::hint::spin_loop();
        }
        unsafe { outb(self.base, byte) };
    }
}

impl Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

pub static CONSOLE: SpinLock<Uart> = SpinLock::new(Uart::new(COM1));

pub fn init() {
    unsafe { CONSOLE.lock().init() };
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    let _ = CONSOLE.lock().write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::serial::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}
```

- [ ] **Step 3: Add the interrupt-control hook and crate root**

```rust
// crates/qunix-hal-x86_64/src/lib.rs
#![no_std]

pub mod port;
pub mod serial;

pub struct Irq;

impl qunix_sync::IrqControl for Irq {
    fn disable_and_save() -> bool {
        let was_enabled = x86_64::instructions::interrupts::are_enabled();
        x86_64::instructions::interrupts::disable();
        was_enabled
    }

    fn restore(was_enabled: bool) {
        if was_enabled {
            x86_64::instructions::interrupts::enable();
        }
    }
}
```

- [ ] **Step 4: Declare Limine requests and call into serial from `kmain`**

```rust
// kernel/src/boot.rs
use limine::BaseRevision;
use limine::request::{HhdmRespData, MemmapRespData, Request};
use limine::{RequestsEndMarker, RequestsStartMarker};

#[used]
#[unsafe(link_section = ".requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static HHDM: Request<HhdmRespData> = Request::new();

#[used]
#[unsafe(link_section = ".requests")]
pub static MEMMAP: Request<MemmapRespData> = Request::new();

#[used]
#[unsafe(link_section = ".requests_start_marker")]
static START_MARKER: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".requests_end_marker")]
static END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

pub fn base_revision_supported() -> bool {
    BASE_REVISION.is_supported()
}
```

```rust
// kernel/src/main.rs
#![no_std]
#![no_main]

mod boot;

use core::panic::PanicInfo;
use qunix_hal_x86_64::println;

#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    qunix_hal_x86_64::serial::init();
    assert!(boot::base_revision_supported(), "limine base revision unsupported");
    println!("qunix: booted");
    halt_forever();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("qunix: PANIC: {info}");
    halt_forever();
}

fn halt_forever() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}
```

Add to `kernel/Cargo.toml`:
```toml
[dependencies]
limine.workspace = true
qunix-hal-x86_64.workspace = true
qunix-sync.workspace = true
```

- [ ] **Step 5: Verify the `limine` 0.6.5 response accessors compile**

Run: `cargo doc -p limine --no-deps --target x86_64-unknown-linux-musl && cargo build -p qunix-kernel`
Expected: build succeeds. `Request::<HhdmRespData>::new()` and `RequestsStartMarker::new()` are `const fn` in 0.6.5. If a constructor name differs, correct it from the generated docs at `target/doc/limine/request/index.html` before continuing — do not proceed with a non-compiling boot module.

- [ ] **Step 6: Write the Limine bootloader config**

```
# limine.conf
timeout: 0

/qunix
    protocol: limine
    kernel_path: boot():/boot/qunix-kernel
```

- [ ] **Step 7: Implement ISO assembly in xtask**

```rust
// xtask/src/image.rs
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

const LIMINE_BRANCH: &str = "v9.x-binary";

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("failed to spawn {cmd:?}"))?;
    if !status.success() {
        bail!("command failed: {cmd:?}");
    }
    Ok(())
}

/// Clones the prebuilt Limine binaries once and builds the host `limine` tool.
pub fn ensure_limine(root: &Path) -> Result<PathBuf> {
    let dir = root.join("target/limine");
    if !dir.exists() {
        run(Command::new("git").args([
            "clone", "--depth", "1", "--branch", LIMINE_BRANCH,
            "https://github.com/limine-bootloader/limine.git",
        ]).arg(&dir))?;
    }
    if !dir.join("limine").exists() {
        run(Command::new("make").arg("-C").arg(&dir))?;
    }
    Ok(dir)
}

/// Builds a hybrid BIOS+UEFI bootable ISO around the kernel ELF.
pub fn build_iso(root: &Path, kernel: &Path) -> Result<PathBuf> {
    let limine = ensure_limine(root)?;
    let iso_root = root.join("target/iso_root");
    let _ = std::fs::remove_dir_all(&iso_root);
    std::fs::create_dir_all(iso_root.join("boot/limine"))?;
    std::fs::create_dir_all(iso_root.join("EFI/BOOT"))?;

    std::fs::copy(kernel, iso_root.join("boot/qunix-kernel"))?;
    std::fs::copy(root.join("limine.conf"), iso_root.join("boot/limine/limine.conf"))?;

    for file in ["limine-bios.sys", "limine-bios-cd.bin", "limine-uefi-cd.bin"] {
        std::fs::copy(limine.join(file), iso_root.join("boot/limine").join(file))
            .with_context(|| format!("copying {file}"))?;
    }
    for file in ["BOOTX64.EFI", "BOOTIA32.EFI"] {
        std::fs::copy(limine.join(file), iso_root.join("EFI/BOOT").join(file))
            .with_context(|| format!("copying {file}"))?;
    }

    let iso = root.join("target/qunix.iso");
    run(Command::new("xorriso").args([
        "-as", "mkisofs", "-R", "-r", "-J",
        "-b", "boot/limine/limine-bios-cd.bin",
        "-no-emul-boot", "-boot-load-size", "4", "-boot-info-table",
        "-hfsplus", "-apm-block-size", "2048",
        "--efi-boot", "boot/limine/limine-uefi-cd.bin",
        "-efi-boot-part", "--efi-boot-image", "--protective-msdos-label",
    ]).arg(&iso_root).args(["-o"]).arg(&iso))?;

    run(Command::new(limine.join("limine")).arg("bios-install").arg(&iso))?;
    Ok(iso)
}
```

- [ ] **Step 8: Implement QEMU invocation in xtask**

```rust
// xtask/src/qemu.rs
use anyhow::{Result, bail};
use std::path::Path;
use std::process::Command;

const OVMF_CANDIDATES: &[&str] = &[
    "/usr/share/edk2/x64/OVMF.4m.fd",
    "/usr/share/edk2-ovmf/x64/OVMF.fd",
    "/usr/share/ovmf/x64/OVMF.fd",
    "/usr/share/OVMF/OVMF_CODE.fd",
];

fn find_ovmf() -> Option<&'static str> {
    if let Ok(path) = std::env::var("QUNIX_OVMF") {
        return Path::new(&path).exists().then(|| Box::leak(path.into_boxed_str()) as &'static str);
    }
    OVMF_CANDIDATES.iter().copied().find(|p| Path::new(p).exists())
}

/// Runs the ISO under QEMU. Returns the raw process exit code.
pub fn run_iso(iso: &Path, headless: bool, uefi: bool) -> Result<i32> {
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.args(["-M", "q35", "-m", "512M", "-cdrom"]).arg(iso);
    cmd.args(["-boot", "d", "-serial", "stdio", "-no-reboot", "-no-shutdown"]);
    cmd.args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]);
    if headless {
        cmd.args(["-display", "none"]);
    }
    if uefi {
        match find_ovmf() {
            Some(path) => {
                cmd.args(["-drive"]);
                cmd.arg(format!("if=pflash,format=raw,readonly=on,file={path}"));
            }
            None => bail!(
                "UEFI requested but no OVMF firmware found; set QUNIX_OVMF or pass --bios"
            ),
        }
    }
    let status = cmd.status()?;
    Ok(status.code().unwrap_or(-1))
}
```

- [ ] **Step 9: Wire up the `run` command**

```rust
// xtask/src/main.rs  (replace the match in `main`)
mod image;
mod qemu;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let release = args.iter().any(|a| a == "--release");
    let uefi = !args.iter().any(|a| a == "--bios");
    let root = workspace_root();

    match args.first().map(String::as_str) {
        Some("build") => {
            let elf = build_kernel(release)?;
            println!("kernel: {}", elf.display());
            Ok(())
        }
        Some("run") => {
            let elf = build_kernel(release)?;
            let iso = image::build_iso(&root, &elf)?;
            let code = qemu::run_iso(&iso, false, uefi)?;
            std::process::exit(code);
        }
        other => bail!("unknown xtask command: {other:?}"),
    }
}
```

- [ ] **Step 10: Boot it and confirm serial output**

Run: `cargo xtask run --bios`
Expected: `qunix: booted` appears on stdout. Terminate QEMU with `Ctrl-A X`.

Then confirm UEFI works: `cargo xtask run`
Expected: same output, booted through OVMF.

- [ ] **Step 11: Commit**

```bash
git add crates/qunix-hal-x86_64 kernel limine.conf xtask
git commit -m "feat(boot): limine boot, 16550 serial console, qemu xtask"
```

---

### Task 4: In-QEMU Test Harness

**Files:**
- Create: `kernel/src/testing.rs`
- Modify: `kernel/src/main.rs`, `kernel/Cargo.toml`, `xtask/src/main.rs`

**Interfaces:**
- Consumes: `qunix_hal_x86_64::port::outl`, `image::build_iso`, `qemu::run_iso`.
- Produces:
  - `testing::exit_qemu(ExitCode) -> !` where `ExitCode::{Success, Failure}`
  - `testing::runner(&[&dyn Testable])` used as `#![test_runner]`
  - `xtask` commands `test` and `runner <elf>`

- [ ] **Step 1: Write the test harness module**

```rust
// kernel/src/testing.rs
use qunix_hal_x86_64::{print, println};

#[derive(Clone, Copy)]
#[repr(u32)]
pub enum ExitCode {
    Success = 0x10,
    Failure = 0x11,
}

/// QEMU's isa-debug-exit device exits the process with `(value << 1) | 1`.
/// Success therefore surfaces on the host as exit status 33.
pub fn exit_qemu(code: ExitCode) -> ! {
    unsafe { qunix_hal_x86_64::port::outl(0xf4, code as u32) };
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

pub trait Testable {
    fn run(&self);
}

impl<T: Fn()> Testable for T {
    fn run(&self) {
        print!("{} ... ", core::any::type_name::<T>());
        self();
        println!("ok");
    }
}

pub fn runner(tests: &[&dyn Testable]) {
    println!("running {} tests", tests.len());
    for test in tests {
        test.run();
    }
    exit_qemu(ExitCode::Success);
}
```

- [ ] **Step 2: Enable the custom test framework in the kernel**

```rust
// kernel/src/main.rs  (replace the attribute block and kmain)
#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(crate::testing::runner)]
#![reexport_test_harness_main = "test_main"]

mod boot;
mod testing;

use core::panic::PanicInfo;
use qunix_hal_x86_64::println;

#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    qunix_hal_x86_64::serial::init();
    assert!(boot::base_revision_supported(), "limine base revision unsupported");
    println!("qunix: booted");

    #[cfg(test)]
    test_main();

    halt_forever();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("FAILED\nqunix: PANIC: {info}");
    testing::exit_qemu(testing::ExitCode::Failure);
}

fn halt_forever() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

#[cfg(test)]
mod tests {
    #[test_case]
    fn harness_runs_at_all() {
        assert_eq!(1 + 1, 2);
    }
}
```

Set `test = true` for the binary in `kernel/Cargo.toml`:
```toml
[[bin]]
name = "qunix-kernel"
path = "src/main.rs"
test = true
```

Do **not** set `harness = false` here. `reexport_test_harness_main` needs cargo's
generated harness entry point to exist so it can be renamed to `test_main`;
disabling the harness makes `test_main` undefined.

- [ ] **Step 3: Implement the `runner` and `test` xtask commands**

```rust
// xtask/src/main.rs  (add these arms to the match in `main`)
        Some("runner") => {
            // Invoked by cargo as the custom-target runner with the test ELF path.
            let elf = PathBuf::from(args.get(1).context("runner requires an ELF path")?);
            let iso = image::build_iso(&root, &elf)?;
            let code = qemu::run_iso(&iso, true, false)?;
            match code {
                33 => Ok(()),                       // ExitCode::Success
                35 => bail!("kernel tests failed"), // ExitCode::Failure
                other => bail!("qemu exited with unexpected status {other}"),
            }
        }
        Some("test") => {
            let mut cmd = Command::new(env!("CARGO"));
            cmd.current_dir(&root);
            cmd.args(["test", "--package", "qunix-kernel"]);
            if !cmd.status()?.success() {
                bail!("kernel tests failed");
            }
            // Host-testable crates are listed explicitly. `--features` is not
            // accepted at the root of a virtual workspace, and the HAL crate
            // cannot build for the host at all, so `--workspace` is not usable.
            for package in ["qunix-sync", "qunix-mm"] {
                let mut host = Command::new(env!("CARGO"));
                host.current_dir(&root);
                host.args([
                    "test", "--target", "x86_64-unknown-linux-musl",
                    "--package", package, "--features", "std",
                ]);
                if !host.status()?.success() {
                    bail!("host tests failed for {package}");
                }
            }
            Ok(())
        }
```

The `runner` arm uses BIOS boot (`uefi = false`) deliberately: it removes the OVMF dependency from the test path so tests run on any machine with QEMU.

- [ ] **Step 4: Run the harness and verify it reports success**

Run: `cargo xtask test`
Expected: serial shows `running 1 tests`, then `qunix_kernel::tests::harness_runs_at_all ... ok`, and the command exits 0.

- [ ] **Step 5: Verify a failing test is actually detected**

Temporarily change the assertion to `assert_eq!(1 + 1, 3);` and run `cargo xtask test`.
Expected: `FAILED`, a panic message, and a non-zero exit status. Revert the change afterwards.

This step is mandatory. A harness that cannot fail is worse than no harness.

- [ ] **Step 6: Commit**

```bash
git add kernel xtask
git commit -m "test: in-qemu test harness via isa-debug-exit"
```

---

### Task 5: GDT and TSS

**Files:**
- Create: `crates/qunix-hal-x86_64/src/gdt.rs`
- Modify: `crates/qunix-hal-x86_64/src/lib.rs`, `kernel/src/main.rs`

**Interfaces:**
- Consumes: `x86_64` crate structures.
- Produces:
  - `qunix_hal_x86_64::gdt::init()`
  - `qunix_hal_x86_64::gdt::DOUBLE_FAULT_IST_INDEX: u16` (value `0`), consumed by Task 6.

- [ ] **Step 1: Write the failing test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn gdt_installs_expected_kernel_code_selector() {
        use x86_64::instructions::segmentation::{CS, Segment};
        qunix_hal_x86_64::gdt::init();
        // Entry 0 is the null descriptor, so kernel code lands at index 1 => 0x08.
        assert_eq!(CS::get_reg().0, 0x08);
    }
```

Add `x86_64.workspace = true` to `kernel/Cargo.toml` dependencies.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find gdt in qunix_hal_x86_64`.

- [ ] **Step 3: Implement the GDT**

```rust
// crates/qunix-hal-x86_64/src/gdt.rs
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
const IST_STACK_SIZE: usize = 4096 * 5;

static mut DOUBLE_FAULT_STACK: [u8; IST_STACK_SIZE] = [0; IST_STACK_SIZE];

static mut TSS: TaskStateSegment = TaskStateSegment::new();
static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable::new();

struct Selectors {
    code: SegmentSelector,
    data: SegmentSelector,
    tss: SegmentSelector,
}

static mut SELECTORS: Option<Selectors> = None;

/// Installs the GDT and TSS on the current CPU.
///
/// Idempotent per CPU; safe to call from tests. Uses `static mut` because this
/// runs before any allocator exists and only ever from a single CPU during M0.
pub fn init() {
    unsafe {
        let stack_start = VirtAddr::from_ptr(&raw const DOUBLE_FAULT_STACK);
        let tss = &mut *(&raw mut TSS);
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            stack_start + IST_STACK_SIZE as u64;

        let gdt = &mut *(&raw mut GDT);
        let code = gdt.append(Descriptor::kernel_code_segment());
        let data = gdt.append(Descriptor::kernel_data_segment());
        let tss_sel = gdt.append(Descriptor::tss_segment(&*(&raw const TSS)));

        gdt.load();
        CS::set_reg(code);
        DS::set_reg(data);
        ES::set_reg(data);
        SS::set_reg(data);
        load_tss(tss_sel);

        SELECTORS = Some(Selectors { code, data, tss: tss_sel });
    }
}

pub fn kernel_code_selector() -> SegmentSelector {
    unsafe { (*(&raw const SELECTORS)).as_ref().expect("gdt not initialised").code }
}

pub fn kernel_data_selector() -> SegmentSelector {
    unsafe { (*(&raw const SELECTORS)).as_ref().expect("gdt not initialised").data }
}

pub fn tss_selector() -> SegmentSelector {
    unsafe { (*(&raw const SELECTORS)).as_ref().expect("gdt not initialised").tss }
}
```

Register the module in `crates/qunix-hal-x86_64/src/lib.rs`:
```rust
pub mod gdt;
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo xtask test`
Expected: PASS.

- [ ] **Step 5: Call it from the boot path**

```rust
// kernel/src/main.rs  (in kmain, after serial::init)
    qunix_hal_x86_64::gdt::init();
    println!("qunix: gdt installed");
```

Since the test also calls `init()`, make the test tolerate the double call by asserting only the resulting selector — which it already does.

- [ ] **Step 6: Verify boot still works**

Run: `cargo xtask test`
Expected: PASS, with `qunix: gdt installed` on serial.

- [ ] **Step 7: Commit**

```bash
git add crates/qunix-hal-x86_64 kernel
git commit -m "feat(hal): gdt with tss and double-fault ist stack"
```

---

### Task 6: IDT and Exception Handlers

**Files:**
- Create: `crates/qunix-hal-x86_64/src/idt.rs`
- Modify: `crates/qunix-hal-x86_64/src/lib.rs`, `kernel/src/main.rs`

**Interfaces:**
- Consumes: `qunix_hal_x86_64::gdt::DOUBLE_FAULT_IST_INDEX`.
- Produces:
  - `qunix_hal_x86_64::idt::init()`
  - `qunix_hal_x86_64::idt::set_handler(vector: u8, handler: extern "x86-interrupt" fn(InterruptStackFrame))` — used by Task 12 for the APIC timer.

- [ ] **Step 1: Write the failing test**

The breakpoint exception is the ideal test: if the handler returns correctly, execution resumes and the test completes. If the IDT is wrong, the machine triple-faults and QEMU exits with a status the runner rejects.

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn breakpoint_exception_returns_to_caller() {
        qunix_hal_x86_64::gdt::init();
        qunix_hal_x86_64::idt::init();
        x86_64::instructions::interrupts::int3();
        // Reaching this line at all is the assertion.
        assert!(true);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find idt in qunix_hal_x86_64`.

- [ ] **Step 3: Implement the IDT**

```rust
// crates/qunix-hal-x86_64/src/idt.rs
use crate::gdt::DOUBLE_FAULT_IST_INDEX;
use crate::println;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

pub type HandlerFn = extern "x86-interrupt" fn(InterruptStackFrame);

static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();

/// Installs the IDT on the current CPU.
pub fn init() {
    unsafe {
        let idt = &mut *(&raw mut IDT);
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.general_protection_fault.set_handler_fn(gp_fault_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(DOUBLE_FAULT_IST_INDEX);
        idt.load();
    }
}

/// Registers a handler for a hardware-interrupt vector.
///
/// # Safety
/// The handler must be valid for the entire life of the system, and `vector`
/// must not collide with an architecturally defined exception vector (0..32).
pub unsafe fn set_handler(vector: u8, handler: HandlerFn) {
    assert!(vector >= 32, "vector {vector} is reserved for exceptions");
    unsafe {
        let idt = &mut *(&raw mut IDT);
        idt[vector].set_handler_fn(handler);
        idt.load();
    }
}

extern "x86-interrupt" fn breakpoint_handler(frame: InterruptStackFrame) {
    println!("qunix: breakpoint at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    panic!("invalid opcode at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn gp_fault_handler(frame: InterruptStackFrame, error_code: u64) {
    panic!(
        "general protection fault (code {error_code:#x}) at {:#x}",
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn page_fault_handler(
    frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let addr = x86_64::registers::control::Cr2::read();
    panic!(
        "page fault at {addr:?} (code {error_code:?}) from {:#x}",
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn double_fault_handler(
    frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    panic!("double fault at {:#x}", frame.instruction_pointer.as_u64());
}
```

`gp_fault_handler` and `page_fault_handler` take an error code, so their signatures differ from `HandlerFn`; that is expected — the `x86_64` crate's setters are typed per vector.

Register the module and re-export the macro path used above:
```rust
// crates/qunix-hal-x86_64/src/lib.rs
pub mod idt;
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo xtask test`
Expected: PASS, with `qunix: breakpoint at 0x...` on serial.

- [ ] **Step 5: Add a double-fault regression test**

An unbounded recursion overflows the kernel stack, which faults on the guard page and, without a working IST, escalates to a triple fault and reboots the machine. With the IST stack in place the double-fault handler panics cleanly, which the harness records as a controlled failure — so this test asserts the *panic path* is reached rather than asserting success.

Rather than wiring a second test binary, verify manually once:

```rust
// kernel/src/main.rs  (temporarily, inside kmain before test_main)
    #[allow(unconditional_recursion)]
    fn overflow() { overflow(); overflow(); }
    overflow();
```

Run: `cargo xtask test`
Expected: serial shows `qunix: PANIC: double fault at 0x...` and QEMU exits 35 — **not** a reboot loop. Remove the temporary code afterwards and re-run `cargo xtask test` to confirm PASS.

- [ ] **Step 6: Call it from the boot path**

```rust
// kernel/src/main.rs  (in kmain, after gdt::init)
    qunix_hal_x86_64::idt::init();
    println!("qunix: idt installed");
```

- [ ] **Step 7: Commit**

```bash
git add crates/qunix-hal-x86_64 kernel
git commit -m "feat(hal): idt with exception handlers and ist double-fault stack"
```

---

### Task 7: Boot Information Extraction

**Files:**
- Modify: `kernel/src/boot.rs`, `kernel/src/main.rs`

**Interfaces:**
- Consumes: `boot::HHDM`, `boot::MEMMAP`.
- Produces:
  - `boot::hhdm_offset() -> u64`
  - `boot::MemoryRegion { start: u64, len: u64, usable: bool }`
  - `boot::usable_regions() -> impl Iterator<Item = MemoryRegion>`

- [ ] **Step 1: Write the failing test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn hhdm_offset_is_in_the_higher_half() {
        let offset = crate::boot::hhdm_offset();
        assert!(offset >= 0xffff_8000_0000_0000, "hhdm offset {offset:#x} is not higher-half");
    }

    #[test_case]
    fn memory_map_reports_usable_memory() {
        let mut regions = 0usize;
        let mut total = 0u64;
        for region in crate::boot::usable_regions() {
            regions += 1;
            total += region.len;
            assert!(region.usable);
            assert!(region.len > 0);
        }
        assert!(regions > 0, "no usable memory regions reported");
        // QEMU is launched with 512 MiB; expect at least 256 MiB usable.
        assert!(total >= 256 * 1024 * 1024, "only {total} bytes usable");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo xtask test`
Expected: FAIL — `cannot find function hhdm_offset`.

- [ ] **Step 3: Implement the accessors**

```rust
// kernel/src/boot.rs  (append)
#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    pub start: u64,
    pub len: u64,
    pub usable: bool,
}

/// Offset of the higher-half direct map installed by Limine.
pub fn hhdm_offset() -> u64 {
    HHDM.response().expect("limine provided no HHDM response").offset()
}

/// Every region the bootloader reported, usable or not.
pub fn all_regions() -> impl Iterator<Item = MemoryRegion> {
    let response = MEMMAP.response().expect("limine provided no memory map");
    response.entries().iter().map(|entry| MemoryRegion {
        start: entry.base,
        len: entry.length,
        usable: entry.entry_type == limine::memmap::EntryType::USABLE,
    })
}

pub fn usable_regions() -> impl Iterator<Item = MemoryRegion> {
    all_regions().filter(|r| r.usable)
}
```

- [ ] **Step 4: Verify the accessor names against the generated docs**

Run: `cargo doc -p limine --no-deps --target x86_64-unknown-linux-musl`
Then open `target/doc/limine/request/struct.HhdmRespData.html` and `target/doc/limine/memmap/index.html`.

Confirm the field and method names used above (`offset()`, `entries()`, `entry.base`, `entry.length`, `entry.entry_type`, `EntryType::USABLE`). Correct them in the code if 0.6.5 names them differently. Do not guess — the compiler error and the docs together give the exact names.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo xtask test`
Expected: PASS.

- [ ] **Step 6: Log the memory map at boot**

```rust
// kernel/src/main.rs  (in kmain, after idt::init)
    let usable: u64 = boot::usable_regions().map(|r| r.len).sum();
    println!("qunix: hhdm at {:#x}, {} MiB usable", boot::hhdm_offset(), usable / (1024 * 1024));
```

- [ ] **Step 7: Commit**

```bash
git add kernel
git commit -m "feat(boot): expose hhdm offset and limine memory map"
```

---

### Task 8: Buddy Frame Allocator (Host-Tested)

The allocator stores its free lists intrusively inside the free frames themselves, so it needs a way to read and write a machine word at a physical address. That access is abstracted behind a trait, which is what makes the allocator host-testable against a `Vec`-backed fake.

**Files:**
- Create: `crates/qunix-mm/Cargo.toml`, `crates/qunix-mm/src/lib.rs`, `crates/qunix-mm/src/buddy.rs`
- Test: `crates/qunix-mm/src/buddy.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: `qunix_sync::SpinLock`.
- Produces:
  - `qunix_mm::FrameBacking` trait with `unsafe fn read_link(&self, pa: u64) -> u64` and `unsafe fn write_link(&self, pa: u64, value: u64)`
  - `qunix_mm::buddy::BuddyAllocator<B: FrameBacking>` with:
    - `const fn new(backing: B) -> Self`
    - `unsafe fn add_region(&mut self, start: u64, len: u64)`
    - `fn alloc(&mut self, order: u8) -> Option<u64>`
    - `unsafe fn free(&mut self, pa: u64, order: u8)`
    - `fn free_bytes(&self) -> u64`
  - `qunix_mm::PAGE_SIZE: u64 = 4096`, `qunix_mm::MAX_ORDER: u8 = 10`

- [ ] **Step 1: Create the crate manifest**

```toml
# crates/qunix-mm/Cargo.toml
[package]
name = "qunix-mm"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
qunix-sync.workspace = true

[features]
default = []
std = []
```

- [ ] **Step 2: Write the failing tests**

```rust
// crates/qunix-mm/src/buddy.rs
#[cfg(test)]
mod tests {
    use super::*;

    /// Host-side backing: a flat byte buffer standing in for physical memory.
    struct VecBacking {
        base: u64,
        mem: std::cell::UnsafeCell<Vec<u8>>,
    }

    impl VecBacking {
        fn new(base: u64, len: usize) -> Self {
            Self { base, mem: std::cell::UnsafeCell::new(vec![0u8; len]) }
        }
    }

    impl FrameBacking for VecBacking {
        unsafe fn read_link(&self, pa: u64) -> u64 {
            let mem = unsafe { &*self.mem.get() };
            let off = (pa - self.base) as usize;
            u64::from_le_bytes(mem[off..off + 8].try_into().unwrap())
        }
        unsafe fn write_link(&self, pa: u64, value: u64) {
            let mem = unsafe { &mut *self.mem.get() };
            let off = (pa - self.base) as usize;
            mem[off..off + 8].copy_from_slice(&value.to_le_bytes());
        }
    }

    fn allocator_with(base: u64, bytes: usize) -> BuddyAllocator<VecBacking> {
        let mut a = BuddyAllocator::new(VecBacking::new(base, bytes));
        unsafe { a.add_region(base, bytes as u64) };
        a
    }

    #[test]
    fn empty_allocator_has_no_free_memory() {
        let a = BuddyAllocator::new(VecBacking::new(0, 0));
        assert_eq!(a.free_bytes(), 0);
        assert_eq!(BuddyAllocator::new(VecBacking::new(0, 0)).free_bytes(), 0);
    }

    #[test]
    fn add_region_accounts_all_whole_pages() {
        let a = allocator_with(0x100000, 16 * 4096);
        assert_eq!(a.free_bytes(), 16 * 4096);
    }

    #[test]
    fn alloc_order_zero_returns_page_aligned_address_in_region() {
        let mut a = allocator_with(0x100000, 16 * 4096);
        let pa = a.alloc(0).expect("allocation failed");
        assert_eq!(pa % PAGE_SIZE, 0);
        assert!(pa >= 0x100000 && pa < 0x100000 + 16 * 4096);
        assert_eq!(a.free_bytes(), 15 * 4096);
    }

    #[test]
    fn alloc_order_two_returns_sixteen_kib_aligned_block() {
        let mut a = allocator_with(0x100000, 16 * 4096);
        let pa = a.alloc(2).expect("allocation failed");
        assert_eq!(pa % (PAGE_SIZE << 2), 0);
        assert_eq!(a.free_bytes(), 12 * 4096);
    }

    #[test]
    fn free_restores_the_original_free_total() {
        let mut a = allocator_with(0x100000, 16 * 4096);
        let before = a.free_bytes();
        let pa = a.alloc(3).unwrap();
        assert_ne!(a.free_bytes(), before);
        unsafe { a.free(pa, 3) };
        assert_eq!(a.free_bytes(), before);
    }

    #[test]
    fn freed_buddies_coalesce_back_into_one_large_block() {
        let mut a = allocator_with(0x100000, 8 * 4096);
        // Drain the whole region as single pages, then give them all back.
        let mut pages = Vec::new();
        while let Some(pa) = a.alloc(0) {
            pages.push(pa);
        }
        assert_eq!(pages.len(), 8);
        assert_eq!(a.free_bytes(), 0);
        for pa in pages {
            unsafe { a.free(pa, 0) };
        }
        assert_eq!(a.free_bytes(), 8 * 4096);
        // If coalescing worked, an order-3 (32 KiB) allocation must now succeed.
        assert!(a.alloc(3).is_some(), "buddies failed to coalesce");
    }

    #[test]
    fn exhausted_allocator_returns_none_rather_than_panicking() {
        let mut a = allocator_with(0x100000, 4 * 4096);
        assert!(a.alloc(5).is_none(), "order-5 request should not fit in 16 KiB");
        for _ in 0..4 {
            assert!(a.alloc(0).is_some());
        }
        assert!(a.alloc(0).is_none());
    }

    #[test]
    fn allocations_never_overlap() {
        let mut a = allocator_with(0x100000, 64 * 4096);
        let mut seen = std::collections::HashSet::new();
        while let Some(pa) = a.alloc(0) {
            assert!(seen.insert(pa), "address {pa:#x} handed out twice");
        }
        assert_eq!(seen.len(), 64);
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p qunix-mm --features std --target x86_64-unknown-linux-musl`
Expected: FAIL — `cannot find type BuddyAllocator`.

- [ ] **Step 4: Implement the allocator**

```rust
// crates/qunix-mm/src/lib.rs
#![cfg_attr(not(test), no_std)]

pub mod buddy;

pub const PAGE_SIZE: u64 = 4096;
pub const MAX_ORDER: u8 = 10; // 4 KiB .. 4 MiB

/// Read/write access to the first machine word of a physical frame.
///
/// The buddy allocator threads its free lists through the frames it manages,
/// so it needs exactly this much access to physical memory and nothing more.
/// The kernel implements it over the HHDM; tests implement it over a `Vec`.
pub trait FrameBacking {
    /// # Safety
    /// `pa` must be a page-aligned address inside a region added to the allocator.
    unsafe fn read_link(&self, pa: u64) -> u64;
    /// # Safety
    /// `pa` must be a page-aligned address inside a region added to the allocator.
    unsafe fn write_link(&self, pa: u64, value: u64);
}
```

```rust
// crates/qunix-mm/src/buddy.rs  (above the tests module)
use crate::{FrameBacking, MAX_ORDER, PAGE_SIZE};

const NIL: u64 = u64::MAX;

/// A binary-buddy physical frame allocator.
///
/// Free blocks of each order form an intrusive singly linked list whose `next`
/// pointer lives in the first word of the block. `NIL` terminates a list.
pub struct BuddyAllocator<B: FrameBacking> {
    backing: B,
    free_lists: [u64; MAX_ORDER as usize + 1],
    free_bytes: u64,
    region_start: u64,
    region_end: u64,
}

impl<B: FrameBacking> BuddyAllocator<B> {
    pub const fn new(backing: B) -> Self {
        Self {
            backing,
            free_lists: [NIL; MAX_ORDER as usize + 1],
            free_bytes: 0,
            region_start: u64::MAX,
            region_end: 0,
        }
    }

    pub fn free_bytes(&self) -> u64 {
        self.free_bytes
    }

    fn block_size(order: u8) -> u64 {
        PAGE_SIZE << order
    }

    fn push(&mut self, pa: u64, order: u8) {
        unsafe { self.backing.write_link(pa, self.free_lists[order as usize]) };
        self.free_lists[order as usize] = pa;
    }

    fn pop(&mut self, order: u8) -> Option<u64> {
        let head = self.free_lists[order as usize];
        if head == NIL {
            return None;
        }
        self.free_lists[order as usize] = unsafe { self.backing.read_link(head) };
        Some(head)
    }

    /// Removes `pa` from the free list of `order`, if present.
    fn unlink(&mut self, pa: u64, order: u8) -> bool {
        let mut cur = self.free_lists[order as usize];
        if cur == NIL {
            return false;
        }
        if cur == pa {
            self.free_lists[order as usize] = unsafe { self.backing.read_link(cur) };
            return true;
        }
        loop {
            let next = unsafe { self.backing.read_link(cur) };
            if next == NIL {
                return false;
            }
            if next == pa {
                let after = unsafe { self.backing.read_link(next) };
                unsafe { self.backing.write_link(cur, after) };
                return true;
            }
            cur = next;
        }
    }

    /// Adds a usable physical region to the allocator.
    ///
    /// # Safety
    /// The region must be genuinely free physical memory that nothing else
    /// owns, and must remain readable and writable through `B` for the life of
    /// the allocator.
    pub unsafe fn add_region(&mut self, start: u64, len: u64) {
        let mut addr = start.next_multiple_of(PAGE_SIZE);
        let end = (start + len) & !(PAGE_SIZE - 1);
        self.region_start = self.region_start.min(addr);
        self.region_end = self.region_end.max(end);

        while addr < end {
            // Take the largest naturally aligned block that still fits.
            let mut order = MAX_ORDER;
            while order > 0 {
                let size = Self::block_size(order);
                if addr % size == 0 && addr + size <= end {
                    break;
                }
                order -= 1;
            }
            self.push(addr, order);
            self.free_bytes += Self::block_size(order);
            addr += Self::block_size(order);
        }
    }

    /// Allocates a naturally aligned block of `PAGE_SIZE << order` bytes.
    pub fn alloc(&mut self, order: u8) -> Option<u64> {
        if order > MAX_ORDER {
            return None;
        }
        // Find the smallest order at or above `order` with a free block.
        let mut source = order;
        while source <= MAX_ORDER && self.free_lists[source as usize] == NIL {
            source += 1;
        }
        if source > MAX_ORDER {
            return None;
        }
        let mut pa = self.pop(source)?;
        // Split downwards, returning the upper half of each split to its list.
        while source > order {
            source -= 1;
            let buddy = pa + Self::block_size(source);
            self.push(buddy, source);
        }
        self.free_bytes -= Self::block_size(order);
        Some(pa)
    }

    /// Returns a block to the allocator, coalescing with its buddy where possible.
    ///
    /// # Safety
    /// `pa` and `order` must exactly match a previous successful `alloc`, and
    /// the memory must no longer be in use.
    pub unsafe fn free(&mut self, pa: u64, order: u8) {
        self.free_bytes += Self::block_size(order);

        let mut pa = pa;
        let mut order = order;
        while order < MAX_ORDER {
            let buddy = pa ^ Self::block_size(order);
            // Only coalesce if the buddy is entirely inside the managed range
            // and currently free at this same order.
            if buddy < self.region_start
                || buddy + Self::block_size(order) > self.region_end
                || !self.unlink(buddy, order)
            {
                break;
            }
            pa = pa.min(buddy);
            order += 1;
        }
        self.push(pa, order);
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p qunix-mm --features std --target x86_64-unknown-linux-musl`
Expected: PASS, 8 tests.

- [ ] **Step 6: Commit**

```bash
git add crates/qunix-mm
git commit -m "feat(mm): buddy physical frame allocator with host tests"
```

---

### Task 9: Wire the Frame Allocator to the Limine Memory Map

**Files:**
- Create: `kernel/src/frames.rs`
- Modify: `kernel/src/main.rs`, `kernel/Cargo.toml`

**Interfaces:**
- Consumes: `boot::{hhdm_offset, usable_regions}`, `qunix_mm::{BuddyAllocator, FrameBacking}`.
- Produces:
  - `frames::init()`
  - `frames::alloc(order: u8) -> Option<u64>`
  - `frames::free(pa: u64, order: u8)` (unsafe)
  - `frames::free_bytes() -> u64`

- [ ] **Step 1: Write the failing test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn frame_allocator_hands_out_usable_physical_memory() {
        crate::frames::init();
        let before = crate::frames::free_bytes();
        assert!(before > 64 * 1024 * 1024, "only {before} bytes of frames");

        let pa = crate::frames::alloc(0).expect("frame allocation failed");
        assert_eq!(pa % 4096, 0);

        // The frame must be readable and writable through the HHDM.
        let va = (crate::boot::hhdm_offset() + pa) as *mut u64;
        unsafe {
            va.write_volatile(0xdead_beef_cafe_f00d);
            assert_eq!(va.read_volatile(), 0xdead_beef_cafe_f00d);
        }

        unsafe { crate::frames::free(pa, 0) };
        assert_eq!(crate::frames::free_bytes(), before);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find frames in the crate root`.

- [ ] **Step 3: Implement the kernel frame allocator**

```rust
// kernel/src/frames.rs
use crate::boot;
use qunix_mm::{FrameBacking, buddy::BuddyAllocator};
use qunix_sync::SpinLock;

/// Reaches physical memory through Limine's higher-half direct map.
struct HhdmBacking {
    offset: u64,
}

impl FrameBacking for HhdmBacking {
    unsafe fn read_link(&self, pa: u64) -> u64 {
        unsafe { ((self.offset + pa) as *const u64).read_volatile() }
    }
    unsafe fn write_link(&self, pa: u64, value: u64) {
        unsafe { ((self.offset + pa) as *mut u64).write_volatile(value) };
    }
}

static ALLOCATOR: SpinLock<Option<BuddyAllocator<HhdmBacking>>> = SpinLock::new(None);

/// Populates the frame allocator from the bootloader memory map.
///
/// Idempotent: a second call is a no-op, which keeps tests independent of order.
pub fn init() {
    let mut guard = ALLOCATOR.lock();
    if guard.is_some() {
        return;
    }
    let offset = boot::hhdm_offset();
    let mut allocator = BuddyAllocator::new(HhdmBacking { offset });

    for region in boot::usable_regions() {
        // Skip the first megabyte: legacy BIOS structures live there and some
        // firmware marks parts of it usable when it is not safe to scribble on.
        let start = region.start.max(0x10_0000);
        if start >= region.start + region.len {
            continue;
        }
        let len = region.start + region.len - start;
        unsafe { allocator.add_region(start, len) };
    }

    *guard = Some(allocator);
}

pub fn alloc(order: u8) -> Option<u64> {
    ALLOCATOR.lock().as_mut().expect("frames::init not called").alloc(order)
}

/// # Safety
/// `pa` and `order` must match a previous successful `alloc`.
pub unsafe fn free(pa: u64, order: u8) {
    unsafe { ALLOCATOR.lock().as_mut().expect("frames::init not called").free(pa, order) };
}

pub fn free_bytes() -> u64 {
    ALLOCATOR.lock().as_ref().map(|a| a.free_bytes()).unwrap_or(0)
}
```

Add `qunix-mm.workspace = true` to `kernel/Cargo.toml`.

- [ ] **Step 4: Register the module and call it at boot**

```rust
// kernel/src/main.rs
mod frames;
```

```rust
// kernel/src/main.rs  (in kmain, after the memory-map log)
    frames::init();
    println!("qunix: {} MiB of frames available", frames::free_bytes() / (1024 * 1024));
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo xtask test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add kernel
git commit -m "feat(mm): back the buddy allocator with the limine memory map"
```

---

### Task 10: Page Table Access

**Files:**
- Create: `crates/qunix-hal-x86_64/src/paging.rs`
- Modify: `crates/qunix-hal-x86_64/src/lib.rs`, `kernel/src/main.rs`

**Interfaces:**
- Consumes: `x86_64` paging types.
- Produces:
  - `qunix_hal_x86_64::paging::AddressSpace` with:
    - `unsafe fn active(hhdm_offset: u64) -> AddressSpace`
    - `unsafe fn map(&mut self, va: u64, pa: u64, flags: PageFlags, frames: &mut impl FnMut() -> Option<u64>) -> Result<(), MapError>`
    - `unsafe fn unmap(&mut self, va: u64) -> Result<u64, MapError>`
    - `fn translate(&self, va: u64) -> Option<u64>`
  - `qunix_hal_x86_64::paging::PageFlags` (bitflags-style newtype with `PRESENT`, `WRITABLE`, `NO_EXECUTE`, `USER`)
  - `qunix_hal_x86_64::paging::MapError`

- [ ] **Step 1: Write the failing test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn mapping_a_fresh_frame_makes_it_readable_and_writable() {
        use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};

        crate::frames::init();
        let hhdm = crate::boot::hhdm_offset();
        let mut space = unsafe { AddressSpace::active(hhdm) };

        let pa = crate::frames::alloc(0).expect("frame allocation failed");
        // A scratch virtual address in an unused part of the higher half.
        const TEST_VA: u64 = 0xffff_9000_0000_0000;

        assert!(space.translate(TEST_VA).is_none(), "test address already mapped");

        unsafe {
            space
                .map(TEST_VA, pa, PageFlags::PRESENT | PageFlags::WRITABLE, &mut || {
                    crate::frames::alloc(0)
                })
                .expect("map failed");
        }

        assert_eq!(space.translate(TEST_VA), Some(pa));

        let ptr = TEST_VA as *mut u64;
        unsafe {
            ptr.write_volatile(0x1234_5678_9abc_def0);
            assert_eq!(ptr.read_volatile(), 0x1234_5678_9abc_def0);
        }

        let unmapped = unsafe { space.unmap(TEST_VA).expect("unmap failed") };
        assert_eq!(unmapped, pa);
        assert!(space.translate(TEST_VA).is_none());

        unsafe { crate::frames::free(pa, 0) };
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find paging in qunix_hal_x86_64`.

- [ ] **Step 3: Implement the address-space wrapper**

```rust
// crates/qunix-hal-x86_64/src/paging.rs
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::mapper::{MapToError, TranslateResult, UnmapError};
use x86_64::structures::paging::{
    FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags,
    PhysFrame, Size4KiB, Translate,
};
use x86_64::{PhysAddr, VirtAddr};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageFlags(u64);

impl PageFlags {
    pub const PRESENT: Self = Self(1 << 0);
    pub const WRITABLE: Self = Self(1 << 1);
    pub const USER: Self = Self(1 << 2);
    pub const NO_EXECUTE: Self = Self(1 << 63);

    fn to_x86(self) -> PageTableFlags {
        let mut flags = PageTableFlags::empty();
        if self.0 & Self::PRESENT.0 != 0 {
            flags |= PageTableFlags::PRESENT;
        }
        if self.0 & Self::WRITABLE.0 != 0 {
            flags |= PageTableFlags::WRITABLE;
        }
        if self.0 & Self::USER.0 != 0 {
            flags |= PageTableFlags::USER_ACCESSIBLE;
        }
        if self.0 & Self::NO_EXECUTE.0 != 0 {
            flags |= PageTableFlags::NO_EXECUTE;
        }
        flags
    }
}

impl core::ops::BitOr for PageFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum MapError {
    OutOfFrames,
    AlreadyMapped,
    NotMapped,
    UnsupportedPageSize,
}

/// Adapts a closure returning physical frame addresses to the `x86_64` crate's
/// `FrameAllocator` trait, so callers are not forced to depend on that crate.
struct ClosureFrames<'a, F: FnMut() -> Option<u64>>(&'a mut F);

unsafe impl<F: FnMut() -> Option<u64>> FrameAllocator<Size4KiB> for ClosureFrames<'_, F> {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        (self.0)().map(|pa| PhysFrame::containing_address(PhysAddr::new(pa)))
    }
}

/// A dummy deallocator: `unmap` in M0 returns the frame to the caller instead
/// of freeing it, so nothing is ever deallocated through this path.
struct NoDealloc;

impl FrameDeallocator<Size4KiB> for NoDealloc {
    unsafe fn deallocate_frame(&mut self, _frame: PhysFrame<Size4KiB>) {}
}

pub struct AddressSpace {
    mapper: OffsetPageTable<'static>,
}

impl AddressSpace {
    /// Wraps the page table currently loaded in CR3.
    ///
    /// # Safety
    /// `hhdm_offset` must be the bootloader's higher-half direct map offset,
    /// and the whole of physical memory must be mapped at that offset.
    pub unsafe fn active(hhdm_offset: u64) -> Self {
        let (frame, _) = Cr3::read();
        let virt = VirtAddr::new(hhdm_offset + frame.start_address().as_u64());
        let table: &'static mut PageTable = unsafe { &mut *virt.as_mut_ptr() };
        let mapper = unsafe { OffsetPageTable::new(table, VirtAddr::new(hhdm_offset)) };
        Self { mapper }
    }

    /// Maps a 4 KiB page.
    ///
    /// # Safety
    /// Creating a mapping can alias memory arbitrarily; the caller owns `pa`
    /// and must ensure `va` is not already in use by something else.
    pub unsafe fn map(
        &mut self,
        va: u64,
        pa: u64,
        flags: PageFlags,
        frames: &mut impl FnMut() -> Option<u64>,
    ) -> Result<(), MapError> {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
        let frame = PhysFrame::containing_address(PhysAddr::new(pa));
        let mut allocator = ClosureFrames(frames);
        let result = unsafe { self.mapper.map_to(page, frame, flags.to_x86(), &mut allocator) };
        match result {
            Ok(flush) => {
                flush.flush();
                Ok(())
            }
            Err(MapToError::FrameAllocationFailed) => Err(MapError::OutOfFrames),
            Err(MapToError::PageAlreadyMapped(_)) => Err(MapError::AlreadyMapped),
            Err(MapToError::ParentEntryHugePage) => Err(MapError::UnsupportedPageSize),
        }
    }

    /// Removes a 4 KiB mapping and returns the physical address it pointed at.
    ///
    /// # Safety
    /// Nothing may hold a reference derived from `va` after this returns.
    pub unsafe fn unmap(&mut self, va: u64) -> Result<u64, MapError> {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
        match self.mapper.unmap(page) {
            Ok((frame, flush)) => {
                flush.flush();
                Ok(frame.start_address().as_u64())
            }
            Err(UnmapError::PageNotMapped) => Err(MapError::NotMapped),
            Err(UnmapError::ParentEntryHugePage) => Err(MapError::UnsupportedPageSize),
            Err(UnmapError::InvalidFrameAddress(_)) => Err(MapError::NotMapped),
        }
    }

    pub fn translate(&self, va: u64) -> Option<u64> {
        match self.mapper.translate(VirtAddr::new(va)) {
            TranslateResult::Mapped { frame, offset, .. } => {
                Some(frame.start_address().as_u64() + offset)
            }
            _ => None,
        }
    }
}
```

Register the module:
```rust
// crates/qunix-hal-x86_64/src/lib.rs
pub mod paging;
```

The `NoDealloc` type is defined for symmetry with `FrameDeallocator` but is not referenced by `unmap`, which hands the frame back to the caller. Delete it if the compiler warns about it being unused — do not add an `#[allow]`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo xtask test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/qunix-hal-x86_64 kernel
git commit -m "feat(hal): page table mapping over the higher-half direct map"
```

---

### Task 11: Slab Heap and `GlobalAlloc`

**Files:**
- Create: `crates/qunix-mm/src/slab.rs`
- Create: `kernel/src/heap.rs`
- Modify: `crates/qunix-mm/src/lib.rs`, `kernel/src/main.rs`

**Interfaces:**
- Consumes: `qunix_mm::PAGE_SIZE`, `frames::alloc`.
- Produces:
  - `qunix_mm::slab::SlabHeap` with:
    - `const fn new() -> Self`
    - `unsafe fn add_backing(&mut self, va: usize, len: usize)`
    - `unsafe fn alloc(&mut self, layout: Layout) -> *mut u8`
    - `unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout)`
    - `fn allocated_bytes(&self) -> usize`
  - `kernel::heap::init()` installing the `#[global_allocator]`

The heap uses fixed size classes (8, 16, 32, 64, 128, 256, 512, 1024, 2048 bytes) with intrusive free lists, and falls back to a bump region for anything larger. That is enough for M0; a real slab with per-class caches and reclaim belongs in a later milestone.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/qunix-mm/src/slab.rs
#[cfg(test)]
mod tests {
    use super::*;
    use core::alloc::Layout;

    /// Gives the heap a real, correctly aligned host allocation to manage.
    fn heap_with(bytes: usize) -> (SlabHeap, Box<[u8]>) {
        let backing = vec![0u8; bytes + 4096].into_boxed_slice();
        let raw = backing.as_ptr() as usize;
        let aligned = (raw + 4095) & !4095;
        let usable = bytes;
        let mut heap = SlabHeap::new();
        unsafe { heap.add_backing(aligned, usable) };
        (heap, backing)
    }

    #[test]
    fn small_allocation_is_correctly_aligned_and_writable() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(24, 8).unwrap();
        let ptr = unsafe { heap.alloc(layout) };
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % 8, 0);
        unsafe { core::ptr::write_bytes(ptr, 0xAB, 24) };
        assert_eq!(unsafe { *ptr }, 0xAB);
    }

    #[test]
    fn allocations_of_the_same_class_do_not_overlap() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(32, 8).unwrap();
        let a = unsafe { heap.alloc(layout) };
        let b = unsafe { heap.alloc(layout) };
        assert!(!a.is_null() && !b.is_null());
        assert_ne!(a, b);
        assert!((a as isize - b as isize).unsigned_abs() >= 32);
    }

    #[test]
    fn freed_block_is_reused_by_the_next_same_sized_allocation() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let first = unsafe { heap.alloc(layout) };
        unsafe { heap.dealloc(first, layout) };
        let second = unsafe { heap.alloc(layout) };
        assert_eq!(first, second, "freed block was not reused");
    }

    #[test]
    fn large_allocation_falls_back_and_still_succeeds() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(9000, 16).unwrap();
        let ptr = unsafe { heap.alloc(layout) };
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % 16, 0);
    }

    #[test]
    fn exhaustion_returns_null_rather_than_panicking() {
        let (mut heap, _backing) = heap_with(8 * 1024);
        let layout = Layout::from_size_align(2048, 8).unwrap();
        let mut succeeded = 0;
        for _ in 0..64 {
            if !unsafe { heap.alloc(layout) }.is_null() {
                succeeded += 1;
            }
        }
        assert!(succeeded > 0, "heap allocated nothing at all");
        assert!(succeeded < 64, "heap never reported exhaustion");
    }

    #[test]
    fn allocated_bytes_tracks_outstanding_allocations() {
        let (mut heap, _backing) = heap_with(64 * 1024);
        let layout = Layout::from_size_align(64, 8).unwrap();
        assert_eq!(heap.allocated_bytes(), 0);
        let ptr = unsafe { heap.alloc(layout) };
        assert_eq!(heap.allocated_bytes(), 64);
        unsafe { heap.dealloc(ptr, layout) };
        assert_eq!(heap.allocated_bytes(), 0);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p qunix-mm --features std --target x86_64-unknown-linux-musl`
Expected: FAIL — `cannot find type SlabHeap`.

- [ ] **Step 3: Implement the heap**

```rust
// crates/qunix-mm/src/slab.rs  (above the tests module)
use core::alloc::Layout;

const CLASSES: [usize; 9] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048];

pub struct SlabHeap {
    free_lists: [*mut u8; CLASSES.len()],
    bump_next: usize,
    bump_end: usize,
    allocated: usize,
}

// The heap is only ever reached through a lock; the raw pointers it holds
// refer to memory it exclusively owns.
unsafe impl Send for SlabHeap {}

impl SlabHeap {
    pub const fn new() -> Self {
        Self {
            free_lists: [core::ptr::null_mut(); CLASSES.len()],
            bump_next: 0,
            bump_end: 0,
            allocated: 0,
        }
    }

    /// Hands the heap a contiguous, mapped, writable virtual region to manage.
    ///
    /// # Safety
    /// `va..va + len` must be mapped, writable, and owned exclusively by the heap.
    pub unsafe fn add_backing(&mut self, va: usize, len: usize) {
        self.bump_next = va;
        self.bump_end = va + len;
    }

    pub fn allocated_bytes(&self) -> usize {
        self.allocated
    }

    fn class_for(layout: Layout) -> Option<usize> {
        if layout.align() > 16 {
            return None;
        }
        CLASSES.iter().position(|&size| size >= layout.size())
    }

    fn bump(&mut self, size: usize, align: usize) -> *mut u8 {
        let start = (self.bump_next + align - 1) & !(align - 1);
        let end = match start.checked_add(size) {
            Some(end) => end,
            None => return core::ptr::null_mut(),
        };
        if end > self.bump_end {
            return core::ptr::null_mut();
        }
        self.bump_next = end;
        start as *mut u8
    }

    /// # Safety
    /// Standard `GlobalAlloc::alloc` contract.
    pub unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        match Self::class_for(layout) {
            Some(class) => {
                let size = CLASSES[class];
                let head = self.free_lists[class];
                let ptr = if head.is_null() {
                    self.bump(size, size.min(16))
                } else {
                    self.free_lists[class] = unsafe { *(head as *mut *mut u8) };
                    head
                };
                if !ptr.is_null() {
                    self.allocated += size;
                }
                ptr
            }
            None => {
                let ptr = self.bump(layout.size(), layout.align().max(16));
                if !ptr.is_null() {
                    self.allocated += layout.size();
                }
                ptr
            }
        }
    }

    /// # Safety
    /// `ptr` and `layout` must match a previous successful `alloc`.
    pub unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        match Self::class_for(layout) {
            Some(class) => {
                unsafe { *(ptr as *mut *mut u8) = self.free_lists[class] };
                self.free_lists[class] = ptr;
                self.allocated -= CLASSES[class];
            }
            None => {
                // Oversized blocks are not recycled in M0. Tracked as a known
                // limitation; the bump region is sized generously to compensate.
                self.allocated -= layout.size();
            }
        }
    }
}

impl Default for SlabHeap {
    fn default() -> Self {
        Self::new()
    }
}
```

Register the module:
```rust
// crates/qunix-mm/src/lib.rs
pub mod slab;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p qunix-mm --features std --target x86_64-unknown-linux-musl`
Expected: PASS, 6 tests.

- [ ] **Step 5: Write the failing kernel-side test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn kernel_heap_supports_box_and_vec() {
        extern crate alloc;
        use alloc::boxed::Box;
        use alloc::vec::Vec;

        crate::frames::init();
        crate::heap::init();

        let boxed = Box::new(0xfeedu32);
        assert_eq!(*boxed, 0xfeed);

        let mut v: Vec<u64> = Vec::new();
        for i in 0..2048 {
            v.push(i);
        }
        assert_eq!(v.len(), 2048);
        assert_eq!(v[2047], 2047);
        assert_eq!(v.iter().sum::<u64>(), (0..2048u64).sum::<u64>());
    }
```

- [ ] **Step 6: Run it to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find heap in the crate root`.

- [ ] **Step 7: Install the global allocator**

```rust
// kernel/src/heap.rs
use crate::{boot, frames};
use core::alloc::{GlobalAlloc, Layout};
use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};
use qunix_mm::slab::SlabHeap;
use qunix_sync::SpinLock;

/// Virtual base of the kernel heap; chosen to sit clear of both the kernel
/// image at -2 GiB and the HHDM.
const HEAP_BASE: u64 = 0xffff_a000_0000_0000;
const HEAP_PAGES: u64 = 4096; // 16 MiB

struct LockedHeap(SpinLock<SlabHeap>);

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { self.0.lock().alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.lock().dealloc(ptr, layout) };
    }
}

#[global_allocator]
static HEAP: LockedHeap = LockedHeap(SpinLock::new(SlabHeap::new()));

static INITIALISED: SpinLock<bool> = SpinLock::new(false);

/// Maps the kernel heap region and hands it to the slab allocator.
///
/// Idempotent, so tests may call it in any order.
pub fn init() {
    let mut done = INITIALISED.lock();
    if *done {
        return;
    }

    let mut space = unsafe { AddressSpace::active(boot::hhdm_offset()) };
    for page in 0..HEAP_PAGES {
        let va = HEAP_BASE + page * qunix_mm::PAGE_SIZE;
        let pa = frames::alloc(0).expect("out of frames while mapping the kernel heap");
        unsafe {
            space
                .map(
                    va,
                    pa,
                    PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NO_EXECUTE,
                    &mut || frames::alloc(0),
                )
                .expect("failed to map a kernel heap page");
        }
    }

    unsafe {
        HEAP.0
            .lock()
            .add_backing(HEAP_BASE as usize, (HEAP_PAGES * qunix_mm::PAGE_SIZE) as usize)
    };
    *done = true;
}
```

Enable `alloc` in the kernel and register the module:
```rust
// kernel/src/main.rs  (near the top, after the attributes)
extern crate alloc;

mod heap;
```

- [ ] **Step 8: Run the test to verify it passes**

Run: `cargo xtask test`
Expected: PASS.

- [ ] **Step 9: Initialise the heap at boot**

```rust
// kernel/src/main.rs  (in kmain, after frames::init)
    heap::init();
    println!("qunix: kernel heap online");
```

- [ ] **Step 10: Commit**

```bash
git add crates/qunix-mm kernel
git commit -m "feat(mm): slab kernel heap and global allocator"
```

---

### Task 12: Local APIC and Timer Interrupt

**Files:**
- Create: `crates/qunix-hal-x86_64/src/apic.rs`
- Modify: `crates/qunix-hal-x86_64/src/lib.rs`, `kernel/src/main.rs`

**Interfaces:**
- Consumes: `boot::hhdm_offset()`, `idt::set_handler`.
- Produces:
  - `qunix_hal_x86_64::apic::init(hhdm_offset: u64)`
  - `qunix_hal_x86_64::apic::start_timer(divide: u32, initial_count: u32)`
  - `qunix_hal_x86_64::apic::eoi()`
  - `qunix_hal_x86_64::apic::TIMER_VECTOR: u8` (value `32`)

- [ ] **Step 1: Write the failing test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn apic_timer_fires_and_advances_the_tick_counter() {
        use core::sync::atomic::Ordering;

        crate::frames::init();
        crate::heap::init();
        qunix_hal_x86_64::gdt::init();
        qunix_hal_x86_64::idt::init();
        crate::install_timer();
        qunix_hal_x86_64::apic::init(crate::boot::hhdm_offset());
        qunix_hal_x86_64::apic::start_timer(0b1011, 10_000_000);

        x86_64::instructions::interrupts::enable();
        let start = crate::TICKS.load(Ordering::Relaxed);
        // Spin until the timer proves it is firing, with a bounded budget so a
        // dead timer fails the test rather than hanging the suite forever.
        let mut budget = 500_000_000u64;
        while crate::TICKS.load(Ordering::Relaxed) == start && budget > 0 {
            core::hint::spin_loop();
            budget -= 1;
        }
        x86_64::instructions::interrupts::disable();

        assert!(budget > 0, "apic timer never fired");
        assert!(crate::TICKS.load(Ordering::Relaxed) > start);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find apic in qunix_hal_x86_64`.

- [ ] **Step 3: Implement the local APIC driver**

```rust
// crates/qunix-hal-x86_64/src/apic.rs
use core::sync::atomic::{AtomicU64, Ordering};

pub const TIMER_VECTOR: u8 = 32;

const IA32_APIC_BASE_MSR: u32 = 0x1B;

// Register offsets, in bytes, from the local APIC base.
const REG_SPURIOUS: usize = 0xF0;
const REG_EOI: usize = 0xB0;
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INITIAL: usize = 0x380;
const REG_TIMER_DIVIDE: usize = 0x3E0;

const LVT_TIMER_PERIODIC: u32 = 1 << 17;
const SPURIOUS_ENABLE: u32 = 1 << 8;
const SPURIOUS_VECTOR: u32 = 0xFF;

static APIC_BASE: AtomicU64 = AtomicU64::new(0);

fn read_msr(msr: u32) -> u64 {
    let (high, low): (u32, u32);
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high,
                         options(nomem, nostack, preserves_flags));
    }
    ((high as u64) << 32) | low as u64
}

fn reg(offset: usize) -> *mut u32 {
    let base = APIC_BASE.load(Ordering::Acquire);
    assert!(base != 0, "apic::init has not been called");
    (base as usize + offset) as *mut u32
}

fn write(offset: usize, value: u32) {
    unsafe { reg(offset).write_volatile(value) };
}

/// Enables the local APIC on the current CPU.
///
/// Reads the APIC base from `IA32_APIC_BASE` and reaches its MMIO window
/// through the higher-half direct map, so no extra mapping is required.
pub fn init(hhdm_offset: u64) {
    let phys_base = read_msr(IA32_APIC_BASE_MSR) & 0xFFFF_F000;
    APIC_BASE.store(hhdm_offset + phys_base, Ordering::Release);
    // Setting the enable bit with a spurious vector is what actually turns the
    // APIC on; without it no LVT entry will ever deliver.
    write(REG_SPURIOUS, SPURIOUS_ENABLE | SPURIOUS_VECTOR);
}

/// Starts the local APIC timer in periodic mode.
///
/// `divide` is the raw divide-configuration value (`0b1011` = divide by 1).
pub fn start_timer(divide: u32, initial_count: u32) {
    write(REG_TIMER_DIVIDE, divide);
    write(REG_LVT_TIMER, LVT_TIMER_PERIODIC | TIMER_VECTOR as u32);
    write(REG_TIMER_INITIAL, initial_count);
}

/// Signals end-of-interrupt. Must be called from every APIC interrupt handler.
pub fn eoi() {
    write(REG_EOI, 0);
}
```

Register the module:
```rust
// crates/qunix-hal-x86_64/src/lib.rs
pub mod apic;
```

- [ ] **Step 4: Add the timer handler in the kernel**

```rust
// kernel/src/main.rs  (at module scope, next to kmain)
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::structures::idt::InterruptStackFrame;

pub static TICKS: AtomicU64 = AtomicU64::new(0);

extern "x86-interrupt" fn timer_handler(_frame: InterruptStackFrame) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    qunix_hal_x86_64::apic::eoi();
}

pub fn install_timer() {
    unsafe { qunix_hal_x86_64::idt::set_handler(qunix_hal_x86_64::apic::TIMER_VECTOR, timer_handler) };
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo xtask test`
Expected: PASS.

- [ ] **Step 6: Start the timer at boot**

```rust
// kernel/src/main.rs  (in kmain, after heap::init)
    install_timer();
    qunix_hal_x86_64::apic::init(boot::hhdm_offset());
    qunix_hal_x86_64::apic::start_timer(0b1011, 10_000_000);
    x86_64::instructions::interrupts::enable();
    println!("qunix: apic timer running");
```

- [ ] **Step 7: Commit**

```bash
git add crates/qunix-hal-x86_64 kernel
git commit -m "feat(hal): local apic with periodic timer interrupt"
```

---

### Task 13: Panic Handler with Stack Backtrace

**Files:**
- Create: `kernel/src/panic.rs`
- Modify: `kernel/src/main.rs`, `kernel/Cargo.toml`, `Cargo.toml`

**Interfaces:**
- Consumes: `testing::exit_qemu`.
- Produces: `panic::backtrace()` printing return addresses walked from the frame-pointer chain.

Backtraces require frame pointers, which are not emitted by default. Enabling them is a build-configuration change, so it is part of this task.

- [ ] **Step 1: Force frame-pointer emission**

```toml
# Cargo.toml  (add to both profiles)
[profile.dev]
panic = "abort"
force-frame-pointers = true

[profile.release]
panic = "abort"
lto = true
force-frame-pointers = true
```

- [ ] **Step 2: Write the failing test**

The backtrace itself cannot be asserted from inside a panic, since the panic ends the test run. Instead, test the walker directly from ordinary code by asserting it reports a plausible chain.

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn backtrace_walks_at_least_one_kernel_frame() {
        #[inline(never)]
        fn depth_two() -> usize {
            let mut frames = 0;
            crate::panic::walk_frames(|addr| {
                // Kernel code is linked at -2 GiB; anything lower is bogus.
                assert!(addr >= 0xffff_ffff_8000_0000, "implausible return address {addr:#x}");
                frames += 1;
            });
            frames
        }
        #[inline(never)]
        fn depth_one() -> usize {
            depth_two()
        }
        assert!(depth_one() >= 2, "backtrace found fewer than two frames");
    }
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find panic in the crate root`.

- [ ] **Step 4: Implement the frame walker and panic handler**

```rust
// kernel/src/panic.rs
use core::panic::PanicInfo;
use qunix_hal_x86_64::println;

const KERNEL_BASE: u64 = 0xffff_ffff_8000_0000;
const MAX_FRAMES: usize = 32;

/// Walks the frame-pointer chain, calling `visit` with each return address.
///
/// Requires `force-frame-pointers = true`; with frame pointers omitted, RBP is
/// a general-purpose register and the chain is meaningless.
pub fn walk_frames(mut visit: impl FnMut(u64)) {
    let mut rbp: u64;
    unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack)) };

    for _ in 0..MAX_FRAMES {
        // A valid frame pointer is higher-half and 8-byte aligned.
        if rbp < KERNEL_BASE || rbp % 8 != 0 {
            break;
        }
        let frame = rbp as *const u64;
        let next_rbp = unsafe { frame.read_volatile() };
        let return_addr = unsafe { frame.add(1).read_volatile() };

        if return_addr < KERNEL_BASE {
            break;
        }
        visit(return_addr);

        // The chain must strictly ascend; anything else means it is corrupt.
        if next_rbp <= rbp {
            break;
        }
        rbp = next_rbp;
    }
}

pub fn print_backtrace() {
    println!("backtrace:");
    let mut index = 0;
    walk_frames(|addr| {
        println!("  {index:>2}: {addr:#018x}");
        index += 1;
    });
    if index == 0 {
        println!("  <no frames recovered>");
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\nqunix: PANIC: {info}");
    print_backtrace();
    crate::testing::exit_qemu(crate::testing::ExitCode::Failure);
}
```

Remove the old `#[panic_handler]` from `kernel/src/main.rs` and register the module:
```rust
// kernel/src/main.rs
mod panic;
```

Having two `#[panic_handler]` functions is a hard compile error, so the old one must go in the same edit.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo xtask test`
Expected: PASS.

- [ ] **Step 6: Verify a real panic prints a usable backtrace**

Temporarily add `panic!("backtrace smoke test");` at the end of `kmain` before `halt_forever()`, then run `cargo xtask test`.
Expected: serial shows the panic message followed by `backtrace:` and at least two `0xffffffff8...` addresses. Resolve one with:
```bash
llvm-addr2line -e target/x86_64-qunix-kernel/debug/qunix-kernel <address>
```
Confirm it points into `kmain`. Remove the temporary panic and re-run `cargo xtask test` to confirm PASS.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml kernel
git commit -m "feat(kernel): panic handler with frame-pointer backtrace"
```

---

## Milestone Exit Criteria

M0 is complete when all of the following hold:

- [x] `cargo xtask build` produces a higher-half ELF for `x86_64-qunix-kernel`.
- [x] `cargo xtask run` boots under UEFI (OVMF) and prints the full init sequence to serial. **BIOS boot is out of scope** -- see deviation 7.
- [x] `cargo xtask test` runs both the host test suites and the in-QEMU suite, and exits zero.
- [x] A deliberately failing in-QEMU test produces a non-zero exit status (verified in Task 4, Step 5).
- [x] A double fault is caught by the IST handler and exits controlled rather than triple-faulting -- verified via the bad-RSP variant, see deviation 13.
- [x] `Box` and `Vec` work in kernel space.
- [x] The APIC timer increments a tick counter.
- [x] A panic prints a backtrace whose addresses resolve to real symbols via `llvm-addr2line`.

## Known Limitations Carried Into M1

These are deliberate and must be addressed by later milestones rather than treated as bugs:

1. **Single CPU.** `gdt::init` and `idt::init` use `static mut` and assume one CPU. M1's SMP work replaces both with per-CPU structures.
2. **No reclaim above 4 MiB.** Extents beyond `LARGE_LISTS` are leaked.
3. **Fixed 16 MiB heap.** The heap is mapped once at init and never grows.
4. **No TLB shootdown.** `AddressSpace::unmap` flushes only the local CPU, which is correct while there is only one CPU and wrong the moment there is not.
5. **Timer period is uncalibrated.** `start_timer` takes a raw count; no relationship to wall-clock time is established until M1 calibrates against the HPET or TSC.
