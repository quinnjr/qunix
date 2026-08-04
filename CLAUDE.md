# qunix — working notes

A bare-metal x86_64 kernel in Rust. This file records what is non-obvious or has
already cost someone an hour. It is not a style guide.

## Commands

```sh
cargo xtask test    # 14 in-QEMU + 51 host tests + the licensing check
cargo xtask run     # interactive boot; a non-test kernel halts and never exits
cargo xtask build
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

- **`no_std`** everywhere except `xtask`.
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

## Gotchas already paid for

- `static mut` access uses `&raw const` / `&raw mut`. Clippy's `deref_addrof`
  suggestion is **wrong** here — taking a direct reference to a `static mut` is a
  hard error under `static_mut_refs` in edition 2024. Those 13 warnings are
  expected; do not "fix" them.
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
process; its plan assumes M0's *planned* interfaces, several of which drifted, so
reconcile it against the code before executing it.
