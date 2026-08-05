# qunix — working notes

A bare-metal x86_64 kernel in Rust. This file records what is non-obvious or has
already cost someone an hour. It is not a style guide.

## Commands

```sh
cargo xtask test    # 46 in-QEMU + 165 host tests + licensing and attestation checks
cargo xtask run     # interactive boot; a non-test kernel halts and never exits
cargo xtask build
cargo xtask bench   # criterion, host-buildable crates only
cargo xtask fuzz    # libFuzzer, 60s per target by default
```

Always go through `xtask`. A bare `cargo build` fails: the kernel needs
`-Zbuild-std`, `-Zjson-target-spec` and `-Zpanic-abort-tests`, which `xtask`
supplies per-invocation. They deliberately do **not** live in
`.cargo/config.toml` — `[unstable] build-std` is global there and would apply to
the host-targeted `xtask` too, rebuilding `core` alongside the precompiled `std`
and failing with `E0152: duplicate lang item`.

To build a single kernel-target crate by hand:

```sh
cargo build -p <crate> -Zbuild-std=core,compiler_builtins,alloc \
  -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec
```

Host crates test against **musl**, not glibc:
`cargo test -p qunix-mm --features std --target x86_64-unknown-linux-musl`.

## Hard rules

- **`no_std`** everywhere except `xtask` and `fuzz/`. Both are host-only tools.
  `fuzz/` is additionally its own workspace, so it is outside `cargo xtask
  test` and the coverage ratchet — `cargo xtask fuzz` is the only thing that
  builds it. The licensing check *does* reach it, because that walks the
  workspace root rather than the member list.
- **No floating point in kernel crates.** The target disables SSE and uses
  soft-float; an `f32` is a bug, not a style choice.
- **Edition 2024 unsafe attributes**: `#[unsafe(no_mangle)]`,
  `#[unsafe(link_section = "…")]`, `#[unsafe(naked)]`. The bare forms do not
  compile.
- **Comments explain *why*.** A comment restating the code is noise; a comment
  recording a non-obvious decision is required. Several bugs here survived
  review because a comment described an earlier version of the code, so when you
  change behaviour, re-read the comment above it.
- **No TODOs, stubs, or "M1 will fix this" markers.** Genuine milestone scoping
  belongs in `docs/superpowers/plans/`, where it is tracked. A comment is not an
  enforcement mechanism — if a future change is *required*, make it a compile
  error, not a note.
- **Every commit leaves `cargo xtask test` green.**

## Tests

The in-QEMU harness signals its verdict through one port write to
`isa-debug-exit`. **Anything that degrades output without halting still passes
green** — dropped console bytes, a truncated panic message, a silent
mis-initialisation. Do not treat a green run as evidence that output was correct.

The failure mode this codebase has actually suffered: *tests that only assert the
positive direction*. Pruning frees, coalescing merges, recycling reuses — all
asserted; that pruning must **refuse** a table still in use, that coalescing must
**refuse** a live buddy, that a forged tag is **rejected** — none asserted, and
two memory-corruption bugs survived a full review inside that gap.

When you add a test, ask what it would take for it to fail. If the answer is
"nothing short of deleting the function", it is not a test yet.

Coverage of contention-dependent code is a property of the host's core count,
not of the code. `SpinLock::lock`'s backoff loop was covered on a 32-core
machine and uncovered on a 4-core CI runner, because with fewer cores
`try_lock` simply succeeded first. If a path only runs under contention, force
the contention — hold the lock in another thread and wait until it is provably
held — rather than relying on the scheduler to produce it.

## Benchmarks

`cargo xtask bench` covers `qunix-mm`, `qunix-sync` and the pure-logic part of
`qunix-hal-x86_64`. The kernel cannot be benchmarked: criterion needs `std` and
the kernel is `no_std` running in QEMU. Hardware paths (port I/O, GDT/IDT/APIC,
real page tables) are out of reach too.

Two traps, both already hit here:

- **The test double can dominate the measurement.** The *benchmark's*
  `VecBacking` was reporting ~35% harness overhead until it was changed from
  bounds-checked slicing to the raw volatile access the kernel actually uses.
  If a benchmark's backing is not shaped like the real one, it measures itself.
  There are three `VecBacking`s — unit tests, benches, fuzz — and they differ
  deliberately; each says so at its definition. Do not unify them.
- **A plausible optimisation can be a regression.** Batching `push`'s three
  adjacent link writes into one `[u64; 3]` store — an obvious win on paper —
  measured 9-17% *slower* and was reverted. Criterion's change detection is the
  reason that was noticed rather than shipped.

## When the ratchet fails, read the lines it names

`qunix-sync` once measured 97.84% here and 96.76% on a CI runner for the *same
commit*, failed the ratchet, then passed on a re-run with no code change. The
tempting conclusions — "the ratchet is flaky", "widen `TOLERANCE_PP`" — were
both wrong.

The cause was the *test*, not the library. `cargo xtask coverage` now lists the
uncovered lines of each regressed crate, and that named `lib.rs:283-284`: the
busy-wait body of the contention test itself. On a runner where the holder
thread was scheduled first, the flag was already set at the first check and the
loop never ran. It is now ordered by a `Barrier` and asserts elapsed time, so a
skipped backoff fails the test instead of silently moving a percentage.

Two things generalise. A test that synchronises by busy-waiting has coverage
that depends on the scheduler, so make the skipped path *fail* rather than
merely go uncovered. And never widen `TOLERANCE_PP` to make a red build pass —
that weakens the ratchet for every crate to accommodate one.

## Fuzzing

`cargo xtask fuzz` runs cargo-fuzz over the buddy allocator and the slab heap.
`cargo xtask fuzz buddy --seconds=300` for one target and a longer budget.
Requires `cargo install cargo-fuzz`.

These allocators will not crash when they are wrong. They are arithmetic over
an array, and the failure this codebase has actually shipped — twice — is
handing the same memory to two callers while every operation returns
successfully. So the targets do not look for panics. They maintain an
independent model of what is live and assert, after every operation, that **no
two live allocations overlap** and that every allocation lies inside a region
that was really added. That is the assertion, not the absence of a crash.

Writing the model is where the work is, and getting it wrong looks exactly like
finding a bug. Three of the first four "crashes" were harness defects:

- `free` deliberately asserts before any region is added, so the harness must
  not call it then.
- `add_region` **silently drops** a range too small to hold a whole page, so a
  model that assumes acceptance will then call `free` and trip that assert.
- `foreign_frees` accumulates *bytes*, not a count of events, despite the name,
  and counts three different rejections. Its docs say so; the name still does
  not.

The fourth was real: `free` validated the buddy's extent against the region but
never the freed block's own, so a block starting inside a region and ending past
it was accepted onto a free list.

A review then found the same hole one step over: the guard checked extent but
not *alignment*, so `free(base + 4096, 1)` was accepted and the next `alloc(1)`
returned an overlapping block. The harness could not have found it — it derived
addresses by rounding down to a multiple of the block size, so a misaligned free
was unreachable by construction. **A fuzz target that normalises its inputs
cannot find bugs in the normalisation.** Both are covered by
`free_refuses_a_block_that_overruns_its_region` and
`free_refuses_a_block_not_aligned_to_its_order`.

The arena in the slab target is a process-lifetime `static`, not a per-run
allocation. The heap hands out interior pointers, so it must outlive every
allocation; `Vec::leak` per run both grows without bound and is reported by
LeakSanitizer.

## Gotchas already paid for

- **`static mut` is gone as of M1 Task 1**, and is banned from here on. The GDT,
  TSS, IDT and double-fault stack it held are per-CPU state, and sharing them
  across CPUs is not a style question — two CPUs faulting onto one IST stack
  corrupt each other. They live in `percpu::PerCpu` now, reached through `GS`.
  The 13 `static_mut_refs` warnings M0 documented as expected are zero; if any
  reappear, something reintroduced a shared mutable static. Use `UnsafeCell` in
  a `Sync` newtype (see `percpu::BspCell`) when a static genuinely cannot be
  allocated, and say who the single writer is.
- **Ring 3 can zero the hidden `GS.base`** with `mov gs, ax`. The syscall stub
  always reloaded it with `swapgs`; the *exception* path did not, so the first
  thing a ring-3 fault handler did was read `gs:[0x20]` at linear address 0x20
  under the faulting process's own tables. Any new entry from ring 3 must
  restore the base before touching per-CPU state — one of two entry paths is
  the shape of hole this kernel keeps finding.
- **CR3 is not a source for the kernel's page-table root.** A user thread
  activates its own space and never switches back, so once a process is running
  the "current" root is that process's. `vmspace::record_kernel_root` captures
  it at boot and every new address space copies from that; reading CR3 instead
  works only by accident, because the kernel halves happen to be identical.
- **`TSS.rsp0` is per-thread, not per-CPU-once.** The scheduler reprograms it
  from the incoming thread's own `kernel_stack_top` before every switch, so a
  ring-3 trap lands on the stack of the thread that is actually running.
  Threads that adopted the boot stack report 0 and are skipped — writing 0
  there would point the next trap at the null page.
- **Pruning must refuse the shared higher half.** PML4 entries 256..512 are
  copied by *reference* into every address space, so the tables beneath them
  belong to no single space and freeing one on teardown frees the kernel's.
  The guard (`paging::shares_the_kernel_half`) is a single bit test, and one
  index either way is a kernel-wide double free.
- The HHDM covers **RAM only**. Device MMIO (the LAPIC at `0xFEE00000`) is not
  mapped and must be mapped explicitly, uncacheable.
- Limine's stack has **no guard page**. Stack overflow scribbles through memory
  instead of faulting, so recursion in the panic path is unbounded by default.
- `-cpu host` is rejected under TCG. Gate it on `/dev/kvm` being *openable*, not
  merely present — the `accel=kvm:tcg` fallback is silent.
- `git checkout <file>` restores from the **index**, not `HEAD`. With work staged
  but uncommitted, that destroys it. This has happened here.

## Licensing

Permissive for qunix's own code, `GPL-2.0` for the Linux compatibility layer.
`cargo xtask test` fails the build if a crate whose name marks it as
Linux-compat code inherits the workspace licence. The syscall personality
(`qunix-linux-abi`) is deliberately exempt — matching UAPI struct layouts is not
the same as reimplementing the in-kernel driver API. See `LICENSING.md`.

## Working on a milestone

Each milestone gets a spec, then a plan, then execution:
`docs/superpowers/specs/` → `docs/superpowers/plans/` → code.

Plans carry an **Execution Deviations** section. When reality contradicts the
plan — and it has, sixteen times in M0 — record it there rather than silently
diverging. A plan written before the code is a hypothesis; the deviations are the
result.

M0 is complete. M1 is SMP, scheduling, address spaces and the first userspace
process. Its plan assumed M0's *planned* interfaces and five of them had
drifted; the reconciliation is written up as Execution Deviation D1 in
`docs/superpowers/plans/2026-08-04-m1-processes.md`. Read that before picking up
a task — in particular, `gdt` exposes selector *accessors* rather than
constants, `AddressSpace::from_root` is what the plan calls `new_empty`, and
`qunix-abi` already exists and is consumed by `xtask`.

**M1 is complete.** Per-CPU state, scheduling policy, context switch, kernel
threads, timer preemption, SMP bring-up, address spaces, the native syscall
ABI, an ELF64 loader, ring 3, and a real init program loaded as a Limine
module. A boot prints `hello from ring 3, qunix` from a process with its own
address space.

A fault taken *from ring 3* kills the offending process; only a fault from ring
0 panics. So a userspace bug now shows up as a process that quietly exits, not
as a stopped machine — assert that the process is gone rather than waiting for
a panic.

`kernel/user/init.s` is assembled by `xtask` (see `userland.rs`) into a
standalone ELF placed in the ESP, not linked into the kernel. The kernel finds
it by module cmdline (`init`), not by index, so adding a second module cannot
silently change which one runs.

QEMU runs with `-smp 4`. The APs come online and park — they do not schedule,
because `sched::Scheduler::current` is one field shared by all CPUs and two
CPUs scheduling through it would put two threads on one stack. Deviation D3 in
the M1 plan says what moving it involves.
