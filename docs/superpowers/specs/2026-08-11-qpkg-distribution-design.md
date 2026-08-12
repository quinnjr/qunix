# qpkg — Arch-sourced package pipeline for the qunix distribution

**Date:** 2026-08-11
**Status:** Approved design, pre-plan

## Purpose

A host-only executable crate, `qpkg`, that turns Arch Linux packaging metadata
into installables for the eventual qunix distribution. It searches the official
x86_64 repositories and the AUR, downloads PKGBUILDs, substitutes the GNU
toolchain for LLVM/clang, cross-builds against qunix's Linux-compat ABI
(`x86_64-unknown-linux-musl`, static), and emits standard `.pkg.tar.zst`
artifacts. Version and build state live in a redb database, which an `update`
command polls against re-synced upstream metadata.

qunix's default shell will be **zsh**: `zsh` and its dependency chain
(`ncurses`, `pcre2`, `zlib`, against musl) are the flagship packages this
pipeline is expected to carry end-to-end. Support beyond that is
**best-effort for any package**: fetch, rewrite, attempt, and report failures
honestly — no curated allowlist.

Note the host/target split: PKGBUILDs are a *bash* format and are always
evaluated by bash on the build host. zsh being qunix's runtime shell is
orthogonal to how packages are built.

## Decisions (settled during brainstorming)

1. **Target:** qunix's Linux-compat ABI now — musl-static x86_64 — not
   host-native builds.
2. **Scope:** best-effort for anything indexed; failures are reported, not
   prevented by curation.
3. **Build engine:** our own PKGBUILD interpreter, not makepkg.
4. **Interpreter style:** bash-assisted — Rust owns the pipeline, real bash
   (spawned, scrubbed environment) does variable extraction and function
   execution. No bash reimplementation in Rust.
5. **Index:** mirror the official repo `.db` archives plus the AUR metadata
   dump into redb; search runs offline; sync is on-demand with a staleness
   warning.
6. **Artifact:** Arch-style `.pkg.tar.zst` with a generated `.PKGINFO`.

## 1. Crate placement and CLI

New top-level workspace member `qpkg/` (binary crate `qpkg`), beside `xtask/`
as the second host-only tool. `crates/*` stays kernel-target-only. CLAUDE.md's
`no_std` rule is amended to name `qpkg` as host-only. Builds with plain
`cargo`; its tests run as host tests under `cargo xtask test`.

CLI (clap):

| Command | Behaviour |
|---|---|
| `qpkg sync` | Refresh the index from one configured mirror + the AUR dump |
| `qpkg search <term>` | Offline search (name/description) against redb; shows repo vs AUR and built-status |
| `qpkg info <name>` | Metadata from the index |
| `qpkg build <name>` | Fetch PKGBUILD → rewrite → cross-build → emit `.pkg.tar.zst` → record in redb |
| `qpkg update` | Auto-sync, then list built packages whose upstream version exceeds the built one (vercmp semantics); `--build` rebuilds them |

## 2. Index and sync (redb)

One redb file at `~/.local/share/qpkg/index.redb` (XDG, overridable via
`--db` / `QPKG_DB`). Tables:

- `packages`: `name` → encoded `{version, repo (core|extra|aur), description,
  url, depends, makedepends, pkgbuild_source}`
- `built`: `name` → `{version_built, pkgbuild_sha256, artifact_path, built_at}`
- `meta`: per-source sync timestamps, mirror URL, schema version

Sync sources:

- Official: `core.db` and `extra.db` (the small metadata tarballs, not
  `.files`) from one configurable mirror; parse `desc` entries.
- AUR: `packages-meta-ext-v1.json.gz` (the full metadata dump), so all of the
  AUR is searchable offline instead of per-query RPC.

Read commands warn — never fail — when the index is older than the staleness
threshold (default 24 h). `update` syncs unconditionally before comparing.

Version comparison implements Arch's vercmp ordering (epoch, pkgrel,
alphanumeric segment rules) natively; it is pure logic and heavily tested.

## 3. Fetch and toolchain rewrite

`build` fetches:

- Official packages: the PKGBUILD (and local files it references) from the
  Arch GitLab packaging repo at the tag matching the indexed version.
- AUR packages: the git snapshot for the package base.

The gcc→LLVM substitution is deliberately **environment injection first, text
surgery last**:

- Injected build environment: `CC=clang`, `CXX=clang++`, `AR=llvm-ar`,
  `RANLIB=llvm-ranlib`, `NM=llvm-nm`, `STRIP=llvm-strip`, `LD=ld.lld`,
  `CHOST=x86_64-unknown-linux-musl`, and `CFLAGS`/`CXXFLAGS`/`LDFLAGS`
  carrying `--target=x86_64-unknown-linux-musl -static` plus the musl sysroot
  paths. The sysroot is the host's `musl` package (`/usr/lib/musl` on Arch);
  `qpkg` verifies it exists at startup of a build and names the package to
  install when it does not, rather than failing mid-compile.
- Textual rewriting only where PKGBUILDs hardcode tools: literal `gcc`/`g++`/
  `ar`/`strip` invocations and `./configure` `--host` handling, via a small
  fixed set of regex rules. Every applied rewrite is logged; `--no-rewrite`
  disables the pass. Rules must not touch look-alikes (`gcc-libs` as a
  dependency name is not a compiler invocation).
- `depends`/`makedepends` are **not installed** by qpkg. It checks the host
  for the required tool baseline (bash, clang/LLVM, git, common build tools)
  and reports missing makedepends by name, best-effort.

## 4. Build execution (bash-assisted interpreter)

Per build: workdir `~/.cache/qpkg/build/<name>/` containing `src/` and `pkg/`.
Every bash invocation is `bash --noprofile --norc` with a scrubbed
environment — only the injected toolchain variables plus `PATH` and a `HOME`
pointed inside the workdir.

1. **Extract.** Source the PKGBUILD, then `declare -p` the standard names
   (`pkgname pkgver pkgrel epoch arch source sha256sums b2sums depends
   makedepends options`) and `declare -f` for functions; parse that output in
   Rust. Refuse the build unless `arch=` contains `x86_64` or `any`.
2. **Sources.** Rust downloads and extracts `source=` entries itself (HTTP via
   `ureq`; git by shelling to `git`) and verifies the checksums. Verification
   is Rust's job precisely so a PKGBUILD cannot skip it.
3. **Functions.** Run `prepare()`, `build()`, `package()` in order, skipping
   absent ones, each as
   `bash -c 'source PKGBUILD && cd "$srcdir" && <fn>'` with `srcdir`/`pkgdir`
   exported. No fakeroot: `pkgdir` is an ordinary directory and ownership is
   normalized to root:root when the tar is written.

Network is reachable only during the source step; the function steps get
`http_proxy`/`https_proxy` pointed at a dead local port as a soft barrier
(a hard namespace cut is explicitly out of scope for this iteration).
A non-zero exit from any function fails the build; full output is captured to
`<workdir>/build.log` and the workdir is left in place for inspection.

`package()` output is checked for **path containment**: any file that
resolves outside `$pkgdir` fails the build.

## 5. Artifact

Generate `.PKGINFO` (pkgname, pkgver including epoch/pkgrel, arch, installed
size, depends) and write `pkg/` as a zstd-compressed GNU tar via the `tar` +
`zstd` crates: entries sorted, ownership root:root, fixed mtime — the archive
is deterministic for identical inputs. Output path:
`~/.local/share/qpkg/artifacts/<name>-<fullver>-x86_64.pkg.tar.zst`; on
success the `built` table is updated.

## 6. Errors and testing

One `thiserror` error enum spanning: network, index corruption, unsupported
arch, source extraction, checksum mismatch, build failure (carrying the log
path and failing stage), rewrite refusal. Best-effort philosophy: report
*which stage* died, never leave the redb `built` table claiming success for a
failed build.

Host tests (all under `cargo xtask test`):

- Repo `.db` parser and AUR JSON parser against committed fixture files.
- vercmp against Arch's documented ordering cases — epoch beats everything,
  `1.0a` vs `1.0.1`, pkgrel tie-breaks.
- PKGBUILD extraction against fixture PKGBUILDs: simple, array-heavy, and one
  with a dynamic `pkgver()`.
- Rewrite rules assert both directions: `gcc` becomes `clang`, **and**
  `gcc-libs` in a dependency array is untouched.
- `.PKGINFO` generation and tar determinism (two runs, identical bytes).
- Negative direction throughout (house rule): a wrong checksum refuses, an
  `arch=(aarch64)` PKGBUILD refuses, a `package()` escaping `$pkgdir` fails
  containment, a corrupt index reports rather than panics.

Network-touching and full-build integration tests sit behind a
`--features online` gate and are excluded from CI.

## Licensing

`qpkg` is qunix's own code: `MIT OR Apache-2.0` (workspace default). It
consumes PKGBUILDs at runtime and ships none; nothing here derives from Linux
kernel source, so the GPL-zone rules do not apply.
