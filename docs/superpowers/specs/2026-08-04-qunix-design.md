# qunix — Design

**Date:** 2026-08-04
**Status:** Approved design; milestones M0–M7 each require their own spec and plan.

## 1. What qunix is

qunix is a monolithic ("macro") kernel written entirely in Rust. Core services —
memory management, scheduling, VFS, networking — live in kernel space, share one
address space, and call each other directly, as in Linux.

qunix diverges from Linux in exactly one structural way: **drivers are not all
in-kernel.** Loadable drivers, including Linux drivers, run in isolated userspace
domains with IOMMU-constrained DMA.

Primary purpose is a learning vehicle. The long-term goal is a full Unix-like
environment with Linux ABI compatibility, so existing applications port easily or
run unmodified.

First target: **x86_64 under QEMU/KVM**, UEFI boot. `no_std` throughout.

## 2. Toolchain and build

- **MSRV:** `rust-version = "1.97"` in `Cargo.toml`.
- **Actual toolchain:** pinned nightly via `rust-toolchain.toml`. Bare-metal targets
  require `-Z build-std` to compile `core`/`alloc` for a custom target, which is
  nightly-only. The MSRV declaration records the language level relied on; it does
  not imply the project builds on stable.
- **Custom target** `x86_64-qunix-kernel.json`: `disable-redzone: true`,
  features `-mmx,-sse,+soft-float`, `code-model: kernel`,
  `panic-strategy: abort`, `relocation-model: static`.
- **Bootloader:** Limine. Provides higher-half mapping, memory map, framebuffer,
  and SMP bootstrap. Hand-rolling UEFI boot is cost without learning payoff that
  cannot be obtained later.
- **Build driver:** `cargo xtask` for build, run, and test.
- **LLVM's role:** build-time backend only — rustc codegen for the kernel, and
  later clang/lld/compiler-rt as the on-system toolchain for ported C applications
  and DKMS modules. LLVM is never embedded in the kernel.
- **Cranelift's role:** all runtime (in-kernel) codegen. See §6, Tier 1.

## 3. Crate topology

Crates are kept small and single-purpose so each has a testable boundary and fits
in working memory.

| Crate | Responsibility |
| --- | --- |
| `qunix-hal` | Architecture traits; keeps the aarch64 port honest before it exists |
| `qunix-hal-x86_64` | GDT/IDT/APIC/paging/context switch/SMP/TSC |
| `qunix-mm` | Buddy frame allocator, virtual memory, slab heap, page cache |
| `qunix-sync` | IRQ-safe spinlocks, wait queues, seqlocks, epoch-based reclamation |
| `qunix-sched` | Threads, per-CPU runqueues, work stealing |
| `qunix-vfs` | vnode/mount abstraction, path resolution, file descriptor tables |
| `qunix-fs-tmpfs`, `-devfs`, `-procfs`, `-ext2` | Filesystem implementations |
| `qunix-abi` | Native qunix syscall definitions, shared kernel↔user |
| `qunix-linux-abi` | Linux personality: syscall table, struct layouts, errno mapping |
| `qunix-driver-core` | Device model, IRQ routing, resource management |
| `qunix-pci` | PCIe enumeration, MSI-X |
| `qunix-iommu` | VT-d domain management |
| `qunix-virtio` | Native virtio-blk/net/console drivers |
| `qunix-jit` | Cranelift-based loader and IR verifier for Tier 1 extensions |
| `qunix-linux-compat` | Linux driver API headers and shim runtime (GPL-2.0, see §9) |
| `qunix-kernel` | Binary: entry point and init |

Userspace: `qdl` (driver domain launcher), `linux-shim` (in-domain `.ko` runtime),
`libqunix`, and a ported libc.

## 4. Memory, scheduling, synchronization

**Memory.** Buddy physical frame allocator; higher-half kernel; per-process page
tables; demand paging; copy-on-write `fork`; `mmap`. The kernel heap is a slab
allocator over the buddy allocator, exposed through `GlobalAlloc`.

**Scheduling.** Preemptive, per-CPU runqueues with work stealing. The thread is
the scheduling unit; a process is an address space plus a file-descriptor table
plus a thread group.

**Synchronization.** Interrupt-aware spinlocks that save and restore IRQ state,
blocking mutexes built on wait queues, seqlocks for read-mostly data, and
epoch-based reclamation for lock-free structures.

## 5. Dual-personality syscall architecture

qunix exposes two syscall ABIs. The governing rule:

> Internal kernel APIs are the single source of truth. Both ABIs are thin
> adapters over them.

```
linux binary ──► linux syscall table ──┐
                                       ├──► kernel::fs::open(), vm::mmap(), ...
qunix binary ──► native syscall table ─┘
```

The Linux layer must **not** translate into native qunix syscalls. Double
translation is where personality layers accumulate semantic drift; both tables
call the same internal functions instead.

**Native ABI.** Handle-based, capability-oriented, own syscall numbering, entered
via `syscall`. Designed for clarity rather than Linux fidelity.

**Linux personality.** Selected per-process at `exec` time from the ELF note or
interpreter path. Implements the x86_64 Linux syscall numbering and semantics.

**Compatibility target order.** Statically linked **musl** binaries first;
`busybox-static` is the milestone goalpost. glibc requires substantially more —
a fuller `/proc`, vDSO, detailed `auxv`, and TLS via `arch_prctl` — and is
deferred. The known-hard areas are `/proc`, signals with exact `sigcontext`
layout, `futex`, `epoll`, and `clone` flag semantics.

## 6. Driver architecture

Three tiers, all converging on the same device model in `qunix-driver-core`.

### Tier 0 — Native in-kernel Rust

virtio, PCIe, serial, timer, IOMMU. Compiled into the kernel, written in safe Rust
where possible.

### Tier 1 — qunix loadable extensions

Modules ship as Cranelift IR (CLIF) or a qunix-specific verified IR. `qunix-jit`
verifies then compiles them to native code in-kernel. Intended for eBPF-class
hooks and simple drivers. LLVM bitcode is explicitly **not** the module format:
LLVM is too large and too hosted-libc-dependent to embed in a `no_std` kernel.

### Tier 2 — Linux drivers in a sandboxed userspace domain

```
┌─ qdl process (one per device) ─────────────────┐
│  linux-shim: loads the .ko ELF relocatable     │
│    implements the Linux driver API against     │
│    qunix primitives: kmalloc, ioremap,         │
│    dma_alloc_coherent, request_irq,            │
│    workqueues, timers, PCI accessors,          │
│    netdev/blockdev registration                │
└──────┬──────────── shared-memory ring ─────────┘
       │ MMIO: kernel maps device BARs into the domain
       │ IRQ:  MSI-X → kernel → ring signal
       │ DMA:  VT-d domain pins only the driver's buffers
       ▼
  qunix-driver-core presents a normal netdev / blkdev
```

The IOMMU constraint is the load-bearing safety property: a compromised or buggy
driver cannot program its device to write outside its own pinned buffers.

**Fault handling.** A domain crash triggers device FLR, IOMMU domain teardown, and
optionally domain restart. A bad driver kills a process, not the machine.

Tier 2 has two front ends.

#### Tier 2a — DKMS source modules (primary)

DKMS builds modules from source, so qunix needs **source-level API compatibility,
not binary ABI compatibility**. This is categorically more tractable:

| | Prebuilt `.ko` | DKMS source module |
| --- | --- | --- |
| Struct layouts | must match Linux exactly, bit for bit | qunix defines them |
| Inline functions and macros | already baked into the object | qunix authors them |
| Kernel config coupling | pinned to one exact `.config` | recompiled per system |
| Version churn | silent breakage | fails loudly at compile time |

qunix therefore ships `/lib/modules/$(uname -r)/build` containing:

- **qunix-authored, API-compatible headers.** They expose Linux API names and
  semantics, but layouts are qunix's own and chosen to map cleanly onto Rust
  types. Modules that reach into undocumented internals will fail to build; that
  is an accepted limitation.
- A **kbuild-compatible Makefile tree** satisfying the standard
  `make -C /lib/modules/$(uname -r)/build M=$PWD modules` invocation, plus a
  generated `autoconf.h`.
- `Module.symvers` for `depmod`.

Supporting surface required: `uname -r` reporting a stable qunix kernel release
string; a `/lib/modules/<release>/` tree; `depmod`, `modprobe`, and `insmod`
reimplemented against `qdl`; module symbol dependency resolution; and DKMS's own
runtime dependencies (POSIX shell, coreutils, `make`, `awk`).

An on-system compiler is required, which is why clang/lld/compiler-rt become the
qunix system toolchain (M3.5). Until that lands, a **cross-DKMS** mode — build on
a host Linux, install onto qunix — unblocks testing.

#### Tier 2b — Prebuilt binary `.ko` (optional, stretch)

Only for drivers with no available source. Requires exact-layout matching against
a pinned Linux version and config, using a separate vendored header set rather
than the Tier 2a headers. Pursued only if a specific binary blob demands it.

**Honest cost note.** The Linux driver API surface for even a modest NIC is
several hundred symbols and drags in the whole `sk_buff` / `net_device` / `napi`
model. Tier 2 is a multi-month subproject and is deliberately not an early
milestone.

## 7. Milestones

Each milestone gets its own spec and implementation plan.

| Milestone | Scope | Done when |
| --- | --- | --- |
| **M0** | Limine boot, serial, GDT/IDT/APIC, paging, buddy + slab, panic and backtrace, QEMU test harness | kernel prints from the higher half; heap allocations work |
| **M1** | Scheduler, SMP, threads, address spaces, native syscalls, ELF loader | first userspace process runs |
| **M2** | VFS, tmpfs, devfs, virtio-blk, ext2 read-only then read-write | mounts and reads a real disk image |
| **M3** | Linux personality: procfs, signals, futex, TLS | `busybox-static` runs |
| **M3.5** | System toolchain: clang, lld, compiler-rt targeting qunix userspace; `make`; a real shell | a C program compiles on qunix and runs |
| **M4** | `qunix-driver-core`, PCIe, IOMMU, userspace driver domain — proved with a *native Rust* driver | virtio-net runs out-of-kernel and still works |
| **M5** | Cranelift extension loader | a loadable IR module binds a device |
| **M6a** | DKMS source modules: Linux-compat header set, kbuild tree, module tooling | a real out-of-tree module completes `dkms build && dkms install` and binds a device |
| **M6b** | Prebuilt binary `.ko` support (optional) | one real `.ko` drives hardware state |
| **M7** | aarch64 port | validates that the HAL boundary was real |

**Ordering rationale.** M4 precedes M6 deliberately: prove the isolation mechanism
with a driver whose behavior is fully understood before introducing the Linux API
as an additional variable.

## 8. Testing strategy

**Host-testable crates.** Pure-logic crates build `no_std` but expose a `std`
feature for host testing: allocators, VFS path resolution, scheduler policy, and
`qunix-linux-abi` struct layouts. Layout mismatches are the most common
personality-layer failure mode, so layouts are diffed against real Linux headers
via bindgen in a host test.

Note the asymmetry with §6: the **syscall personality** (`qunix-linux-abi`) must
match Linux UAPI layouts exactly, because unmodified binaries pass those structs
across the syscall boundary. The **driver API** (`qunix-linux-compat`) need not,
because modules are recompiled from source against qunix's own headers. Exact
layout matching is required in one layer and explicitly avoided in the other.

**In-QEMU integration tests.** Custom test harness using `isa-debug-exit` with
serial capture, driven by `cargo xtask test`.

**ABI conformance.** Identical test binaries run on Linux and on qunix; syscall
traces are diffed.

**Driver-domain fault injection.** A domain is killed mid-DMA; the test asserts
both that the kernel survives and that the IOMMU blocked stray writes. This test
is the entire argument for the driver architecture and exists from M4 onward.

## 9. Licensing

- qunix kernel and core crates: permissive (MIT or Apache-2.0).
- `qunix-linux-compat`, `linux-shim`, and the Linux-compatible header set:
  **GPL-2.0**.

Shipping Linux-API-compatible headers and a shim runtime places that layer in
GPL-derivative-work territory. The split is deliberate: it keeps the kernel core
reusable while treating the compatibility layer conservatively.

## 10. Risks

1. **Cranelift in `no_std`.** Its `no_std` plus `alloc` support must be verified
   against the current release with a spike before M5 is committed, not assumed.
2. **Linux API churn.** Because qunix authors its own headers (Tier 2a), churn
   surfaces as compile failures in third-party modules rather than silent
   breakage. The mitigation is to track a specific upstream API generation and
   document which one the headers target.
3. **Toolchain port scope.** M3.5 (clang/lld/compiler-rt on qunix) is a large
   milestone in its own right and gates on-system DKMS. Cross-DKMS is the
   mitigation.
4. **Overall scope.** M0–M3 is already a serious project on its own. M6 is where
   comparable efforts historically stall.

## 11. Open questions

None outstanding. Decisions recorded above: sandboxed userspace driver domains
with IOMMU; dual-personality syscalls with musl-first targeting; Cranelift for
in-kernel codegen and LLVM strictly at build time; qunix-authored Linux-compatible
headers; permissive core with GPL-2.0 compatibility crates.
