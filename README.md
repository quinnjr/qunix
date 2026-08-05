# qunix

A monolithic operating system kernel written in Rust, targeting x86_64.

Core services — memory management, scheduling, VFS, networking — live in kernel
space and call each other directly, as in Linux. qunix diverges in exactly one
structural way: **loadable drivers do not run in the kernel.** Linux drivers run
in isolated userspace domains with IOMMU-constrained DMA, so a driver fault kills
a process rather than the machine.

The long-term goal is a Unix-like environment with Linux ABI compatibility, so
existing applications port easily or run unmodified.

> **Status: milestone M0 complete.** The kernel boots under UEFI, brings up
> descriptor tables and the local APIC, and runs a buddy frame allocator and a
> kernel heap — `Box` and `Vec` work. There is no scheduler, no userspace, and no
> filesystem yet; those are M1 and M2.

## Quickstart

```sh
cargo xtask run     # boot under QEMU
cargo xtask test    # 103 tests: 14 in-QEMU, 89 on the host
cargo xtask build   # kernel ELF only
```

The first run clones a pinned Limine build into `.limine/` and verifies its
SHA-256. Expect a boot to look like:

```
qunix: booted
qunix: gdt installed
qunix: idt installed
qunix: hhdm at 0xffff800000000000, 458 MiB of frames available (...)
qunix: kernel heap online (16384 KiB bump headroom)
qunix: apic timer running
```

### Requirements

- `qemu-system-x86_64`
- OVMF firmware — qunix boots UEFI-only. Auto-detected at the usual distro
  paths; override with `QUNIX_OVMF=/path/to/OVMF.fd`.
- `git`

The Rust toolchain pins itself via `rust-toolchain.toml` (a specific nightly —
`-Z build-std` is required for a custom bare-metal target and is nightly-only).

KVM is used when `/dev/kvm` is openable and falls back to emulation otherwise;
the difference is roughly 10x on the test loop.

## Layout

| Crate | |
| --- | --- |
| `qunix-abi` | native syscall numbers; the kernel↔host test contract |
| `qunix-sync` | spinlocks and interrupt-safe spinlocks |
| `qunix-mm` | buddy frame allocator, size-class kernel heap |
| `qunix-hal-x86_64` | GDT/IDT/APIC/paging/serial |
| `qunix-kernel` | the kernel binary |
| `xtask` | build, image assembly, QEMU orchestration |

`qunix-sync`, `qunix-mm` and `qunix-hal-x86_64` build for both the kernel target
and the host, so their logic is unit-tested natively; hardware-dependent
behaviour is tested inside QEMU, with the verdict signalled through the
`isa-debug-exit` device.

## Design

Two decisions shape everything else.

**Drivers are sandboxed, not trusted.** Linux drivers run in a userspace domain
per device, reached over a shared-memory ring, with the kernel programming an
IOMMU domain that confines the device to that driver's pinned buffers. That
IOMMU constraint is the load-bearing safety property — without it the isolation
is decorative.

**Linux compatibility is source-level, not binary.** DKMS builds modules from
source, so qunix needs to implement a Linux *API*, not reverse-engineer its ABI.
Struct layouts become qunix's own rather than something to match bit-for-bit.
Prebuilt `.ko` support is a stretch goal, not the path.

Full design: [`docs/superpowers/specs/2026-08-04-qunix-design.md`](docs/superpowers/specs/2026-08-04-qunix-design.md).
Implementation plans, including what M0 actually delivered versus what was
planned, are in [`docs/superpowers/plans/`](docs/superpowers/plans/).

## Roadmap

| | | |
| --- | --- | --- |
| **M0** | boot, descriptor tables, APIC, allocators, heap | **done** |
| M1 | SMP, scheduler, address spaces, syscalls, first userspace process | next |
| M2 | VFS, tmpfs, devfs, virtio-blk, ext2 | |
| M3 | Linux syscall personality — `busybox-static` runs | |
| M4 | driver core, PCIe, IOMMU, userspace driver domain | |
| M5 | Cranelift-based loadable extensions | |
| M6 | DKMS source modules against qunix-authored headers | |
| M7 | aarch64 port | |

## Known limitations

These are deliberate and scoped to later milestones, not oversights:

- Single CPU. `gdt::init`/`idt::init` assume it, and the tick counter has one writer.
- Fixed 16 MiB heap that never grows; extents above 4 MiB are not recycled.
- No TLB shootdown — correct while there is one CPU, wrong the moment there is not.
- The buddy allocator's free-list membership is tracked in-band, inside the
  frames it manages. Links are bounds-checked, but moving membership out of band
  is required before any frame reaches userspace or a DMA-capable device.

## Licence

Permissive (`MIT OR Apache-2.0`) for qunix's own code; `GPL-2.0` for the Linux
compatibility layer when it lands. The split is enforced at build time — see
[`LICENSING.md`](LICENSING.md).
