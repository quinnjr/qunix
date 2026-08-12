# qpkg Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A host-only `qpkg` binary that indexes Arch repos + AUR into redb, fetches PKGBUILDs, rewrites gcc→LLVM, cross-builds to musl-static x86_64, and emits `.pkg.tar.zst`.

**Architecture:** Rust owns the pipeline (index, fetch, checksum, rewrite, packaging); real bash — spawned with a scrubbed environment — evaluates PKGBUILD variables (`declare -p`) and runs `prepare`/`build`/`package`. redb holds `packages`, `built`, `meta` tables; sync mirrors `core.db`/`extra.db` plus the AUR metadata dump.

**Tech Stack:** clap (derive), redb, ureq, serde/serde_json, thiserror, tar, zstd, flate2, sha2, blake2, hex.

**Spec:** `docs/superpowers/specs/2026-08-11-qpkg-distribution-design.md`

## Global Constraints

- `qpkg/` is a **host-only** top-level workspace member, like `xtask`. Plain cargo, no `-Zbuild-std`. Tests run on the **host triple**, not musl (zstd-sys is C; the musl cross C toolchain is not a test dependency worth taking).
- No floating point restrictions, no `no_std` — this crate is exempt, and CLAUDE.md must say so.
- Workspace licence (`MIT OR Apache-2.0`); the licensing check must stay green.
- Network-touching tests live behind `--features online` and are excluded from `cargo xtask test`.
- Every test asserts a negative direction where one exists (house rule).
- Every commit leaves `cargo xtask test` green.

---

### Task 1: Crate scaffold, workspace wiring, xtask hookup

**Files:**
- Create: `qpkg/Cargo.toml`, `qpkg/src/main.rs`, `qpkg/src/cli.rs`, `qpkg/src/error.rs`, `qpkg/src/paths.rs`
- Modify: `Cargo.toml` (workspace members), `xtask/src/main.rs:275-303` (add a qpkg host-test invocation), `CLAUDE.md` (no_std exemption list)

**Interfaces:**
- Produces: `Error` (thiserror enum, one variant per spec §6 failure class), `Result<T> = std::result::Result<T, Error>`; `paths::{db_path(), artifacts_dir(), build_dir(name)}` honoring `QPKG_DB`/XDG.

- [ ] Add `"qpkg"` to `members` in the root `Cargo.toml`.
- [ ] `qpkg/Cargo.toml`: package qpkg, workspace lints/edition/licence, deps `clap = { version = "4", features = ["derive"] }`, `redb = "2"`, `ureq = "2"`, `serde = { version = "1", features = ["derive"] }`, `serde_json = "1"`, `thiserror = "2"`, `tar = "0.4"`, `zstd = "0.13"`, `flate2 = "1"`, `sha2 = "0.10"`, `blake2 = "0.10"`, `hex = "0.4"`; `[features] online = []`.
- [ ] `error.rs`: enum with variants `Network(String)`, `Index(String)`, `UnsupportedArch { name: String, arches: Vec<String> }`, `Extraction(String)`, `ChecksumMismatch { file: String, expected: String, got: String }`, `Build { stage: &'static str, log: PathBuf }`, `Containment(PathBuf)`, `MissingTool(String)`, `Io(#[from] std::io::Error)`.
- [ ] `cli.rs`: clap derive with subcommands `Sync`, `Search { term }`, `Info { name }`, `Build { name, no_rewrite: bool }`, `Update { build: bool }`, global `--db <path>`.
- [ ] `paths.rs`: `db_path()` = `$QPKG_DB` else `$XDG_DATA_HOME|~/.local/share` + `qpkg/index.redb`; `artifacts_dir()`, `build_dir(name)` under `~/.cache/qpkg/build/`.
- [ ] `main.rs`: parse CLI, dispatch to `todo-free` stubs that return a clear "not yet wired" error only during this task's commit window — replaced by the end of the plan (no TODO markers; the match arms call functions that exist).
- [ ] Test (in `paths.rs`): `db_path` honors `QPKG_DB`; negative: unset `XDG_DATA_HOME` + set `HOME` yields the `~/.local/share` path, and an empty `QPKG_DB` is ignored.
- [ ] `xtask/src/main.rs`: after the musl host-test invocation, add a second `cargo test -p qpkg` invocation (host triple, no `--target`), same `--release` propagation, `bail!("qpkg tests failed")` on failure.
- [ ] `CLAUDE.md` hard rules: amend the `no_std` bullet to name `qpkg` alongside `xtask` and `fuzz/`.
- [ ] Run `cargo xtask test`; commit `feat(qpkg): scaffold the host-side package pipeline crate`.

### Task 2: vercmp

**Files:**
- Create: `qpkg/src/vercmp.rs`

**Interfaces:**
- Produces: `pub fn vercmp(a: &str, b: &str) -> Ordering`; full versions are `[epoch:]pkgver[-pkgrel]`.

- [ ] Tests first, from Arch's documented ordering: `1.0 < 1.0.1`, `1.0a < 1.0.1` (alpha segment < numeric), `1.0 < 1.0a`? — no: per rpmvercmp, `1.0a < 1.0.1` and `1.0 < 1.0a`; numeric segments compare as integers (`10 > 9`, leading zeros ignored); `1:0.1 > 999.9` (epoch dominates); pkgrel tie-break `1.0-2 > 1.0-1`; missing pkgrel equal to present (`1.0` == `1.0-1` treated equal when one side lacks rel); negative: `vercmp("1.0","1.0")` is `Equal`, and `!=` for `1.0` vs `1.01`... assert `1.01 == 1.1`? No — rpmvercmp: `1.01` vs `1.1` → numeric 01 vs 1 → equal. Include that as an explicit case.
- [ ] Implement segment-walk: split alpha/numeric runs, numeric > alpha, longer-numeric wins, tail rules (`1.0` < `1.0.`? follow pacman: more segments of same prefix wins if next seg is numeric, loses if alpha).
- [ ] Run, pass, commit `feat(qpkg): implement pacman version ordering`.

### Task 3: Index (redb layer)

**Files:**
- Create: `qpkg/src/index.rs`

**Interfaces:**
- Produces:
  - `PackageRecord { name, version, repo: Repo, description, url, depends: Vec<String>, makedepends: Vec<String> }`, `Repo::{Core, Extra, Aur}`
  - `BuiltRecord { version_built, pkgbuild_sha256, artifact_path, built_at_unix }`
  - `Index::open(path) -> Result<Index>`; `upsert_packages(&[PackageRecord])`, `get(name)`, `search(term) -> Vec<PackageRecord>` (substring on name/description, name-matches first), `record_built(BuiltRecord)`, `built(name)`, `all_built()`, `set_sync_time(source, unix)`, `sync_age(source)`, `stale(threshold) -> bool`.
- Values stored as `serde_json` bytes under `TableDefinition<&str, &[u8]>`.

- [ ] Tests (tempdir db): roundtrip upsert/get; search finds by description substring and ranks exact-name first; `record_built` then `built()` roundtrip; negative: `get` on missing name is `None`, `open` on a garbage file returns `Error::Index` (not a panic), and a `PackageRecord` written by an older schema field-set still decodes (unknown fields tolerated via serde default) — assert a record missing `url` decodes with empty string.
- [ ] Implement over redb; a corrupt DB maps `redb::DatabaseError` into `Error::Index`.
- [ ] Run, pass, commit `feat(qpkg): redb-backed package index`.

### Task 4: Repo .db parser

**Files:**
- Create: `qpkg/src/repodb.rs`, fixture `qpkg/tests/fixtures/mini-core.db` (built by the test itself in-memory — no binary blob in git: construct a gzipped tar with two `<name>-<ver>/desc` entries using the `tar` + `flate2` crates inside the test)

**Interfaces:**
- Produces: `pub fn parse_repo_db(reader: impl Read, repo: Repo) -> Result<Vec<PackageRecord>>` — sniffs gzip (`1f 8b`) vs zstd (`28 b5 2f fd`) magic, reads `*/desc` entries (`%NAME%`, `%VERSION%`, `%DESC%`, `%URL%`, `%DEPENDS%`, `%MAKEDEPENDS%` blocks).

- [ ] Tests: build a synthetic `.db.tar.gz` in-memory with two packages (one with depends/makedepends, one minimal) → parse → assert both records exact. Negative: a tar entry that is not `*/desc` is ignored; a desc missing `%NAME%` is skipped with the rest still parsed; zstd-compressed variant of the same archive parses identically (assert magic-sniff, not extension).
- [ ] Implement: magic sniff → `flate2::read::GzDecoder` or `zstd::Decoder` → `tar::Archive`, block parser as a line state machine.
- [ ] Run, pass, commit `feat(qpkg): parse pacman repository databases`.

### Task 5: AUR dump parser

**Files:**
- Create: `qpkg/src/aur.rs`

**Interfaces:**
- Produces: `pub fn parse_aur_dump(reader: impl Read) -> Result<Vec<PackageRecord>>` — gzipped JSON array of objects with `Name`, `Version`, `Description` (nullable), `URL` (nullable), `Depends`/`MakeDepends` (optional arrays); repo = `Repo::Aur`. Also `pub fn snapshot_url(base: &str) -> String` for `https://aur.archlinux.org/cgit/aur.git/snapshot/<base>.tar.gz`.

- [ ] Tests: literal JSON fixture string (3 entries: full, nulls, missing arrays) gz-compressed in the test → parse → exact records; negative: malformed JSON is `Error::Index` with context, not a panic; a null `Description` becomes `""`.
- [ ] Implement with `serde_json` + `flate2`.
- [ ] Run, pass, commit `feat(qpkg): parse the AUR metadata dump`.

### Task 6: sync, search, info commands

**Files:**
- Create: `qpkg/src/sync.rs`, `qpkg/src/commands.rs`
- Modify: `qpkg/src/main.rs` (wire Sync/Search/Info)

**Interfaces:**
- Consumes: `parse_repo_db`, `parse_aur_dump`, `Index`.
- Produces: `sync::run(&Index, &SyncConfig) -> Result<SyncReport>` where `SyncConfig { mirror: String /* default https://geo.mirror.pkgbuild.com */, aur_dump_url: String }`; `fetch(url) -> Result<Vec<u8>>` isolated in `sync::http` so tests stub it; `commands::{search, info}` printing to a `impl Write` for testability.
- Staleness: read paths call `index.stale(24h)` and print a warning to stderr; never fail.

- [ ] Tests: `sync::ingest(index, core_bytes, extra_bytes, aur_bytes)` (the pure part, split from HTTP) populates all three repos and stamps sync times; re-ingest with a newer version upserts (assert version replaced, count unchanged); `search` output lists `[core]`/`[aur]` tags and a `built` marker when the built table has the name; negative: `info` on an unknown name prints a not-found message and exits nonzero; stale index emits the warning (capture stderr writer), fresh one does not.
- [ ] `#[cfg(feature = "online")]` integration test: real sync against the default mirror, assert >1000 packages land.
- [ ] Implement; HTTP via `ureq` with a 60 s timeout, `Error::Network` on failure.
- [ ] Run, pass, commit `feat(qpkg): sync, search and info against the mirrored index`.

### Task 7: PKGBUILD extraction (bash-assisted)

**Files:**
- Create: `qpkg/src/pkgbuild.rs`, fixtures `qpkg/tests/fixtures/PKGBUILD.{simple,arrays,dynver}`

**Interfaces:**
- Produces: `Pkgbuild { pkgname: Vec<String>, pkgver, pkgrel, epoch: Option<String>, arch: Vec<String>, source: Vec<String>, sha256sums: Vec<String>, b2sums: Vec<String>, depends, makedepends, options: Vec<String>, functions: BTreeSet<String> }`; `pub fn extract(path: &Path) -> Result<Pkgbuild>`; `Pkgbuild::full_version()` (`[epoch:]ver-rel`), `Pkgbuild::check_arch() -> Result<()>` (x86_64 or any).
- Extraction runs `bash --noprofile --norc` with env scrubbed to `PATH`,`HOME` (workdir), sourcing the file then `declare -p <vars> 2>/dev/null; declare -F`.

- [ ] Tests: simple fixture (scalar vars) extracts exactly; array fixture (`arch=(x86_64 aarch64)`, multi-source, `depends=('zlib>=1.2' ncurses)`) extracts arrays in order; dynver fixture defines `pkgver()` and `build()` — `functions` contains both; negative: `check_arch` on `arch=(aarch64)` returns `Error::UnsupportedArch`; a PKGBUILD that `exit 1`s on source returns `Error::Extraction`; a value containing `$(date)` is whatever bash evaluated — assert extraction does NOT run in the caller's env by planting `HOME`-dependent content and checking the scrubbed value.
- [ ] Implement `declare -p` output parsing: `declare -- name="v"` scalars, `declare -a name=([0]="a" [1]="b")` arrays (bash quoting: handle `\"` and `\\` escapes; single-quoted `$'...'` forms rejected with `Error::Extraction` naming the variable).
- [ ] Run, pass, commit `feat(qpkg): evaluate PKGBUILD metadata through scrubbed bash`.

### Task 8: Sources — fetch, verify, extract

**Files:**
- Create: `qpkg/src/sources.rs`

**Interfaces:**
- Consumes: `Pkgbuild`.
- Produces: `pub fn stage(pb: &Pkgbuild, workdir: &Path, fetch: &dyn Fn(&str) -> Result<Vec<u8>>) -> Result<()>` — parses each `source=` entry (`[dest::]url` and bare filenames; `git+<url>[#tag=..|#commit=..]` shells to `git clone`/`checkout`), writes into `workdir/src/`, verifies `sha256sums`/`b2sums` positionally (`SKIP` allowed), extracts recognized archives (`.tar.*` via tar+sniffed decompressor) unless listed in `noextract` — YAGNI: `noextract` omitted, extraction is by suffix.
- Checksum verification is Rust's job; a mismatch is fatal before any function runs.

- [ ] Tests (fetch stubbed with an in-test closure): a `file.tar.gz` source is fetched, checksum-verified, extracted (assert an inner file exists under `src/`); a plain file source is copied not extracted; `SKIP` skips verification; negative: wrong sha256 → `Error::ChecksumMismatch` and `src/` does not contain the extracted tree; a source count ≠ checksum count → `Error::Extraction`.
- [ ] Implement; hashing via `sha2::Sha256`/`blake2::Blake2b512`.
- [ ] Run, pass, commit `feat(qpkg): stage and verify PKGBUILD sources`.

### Task 9: Toolchain — environment + rewrite

**Files:**
- Create: `qpkg/src/toolchain.rs`

**Interfaces:**
- Produces: `pub fn build_env(workdir: &Path) -> Vec<(String, String)>` — the spec §3 set (`CC=clang`, …, `CHOST=x86_64-unknown-linux-musl`, `CFLAGS/CXXFLAGS/LDFLAGS` with `--target=x86_64-unknown-linux-musl -static` and musl sysroot flags, dead-port proxies for the function phase); `pub fn rewrite(pkgbuild_text: &str) -> (String, Vec<RewriteLog>)` with `RewriteLog { line: usize, rule: &'static str, before: String, after: String }`; `pub fn check_host() -> Result<()>` (clang, llvm-ar, ld.lld, bash, git on PATH; `/usr/lib/musl` present → else `Error::MissingTool("musl")`).
- Rewrite rules (fixed, ordered): word-boundary `gcc`→`clang`, `g++`→`clang++`, `\bar \b`→`llvm-ar ` only at command position (start-of-line/after `|`,`;`,`&&`,`$( `), `strip `→`llvm-strip `, `make CC=gcc`-style assignments rewritten.

- [ ] Tests: `gcc -o x x.c` in a build() body rewrites to clang and logs line+rule; `./configure CC=gcc` rewrites; negative (the important half): `gcc-libs` inside `depends=()` untouched; `agcc`/`gccgo` untouched; a comment line `# needs gcc 12` untouched (rules skip lines whose first non-space is `#`); `build_env` values contain `--target=x86_64-unknown-linux-musl` and `-static`; `rewrite` with no matches returns the input byte-identical.
- [ ] Implement with hand-rolled line scanning (no regex dep; the rules are word-boundary checks).
- [ ] Run, pass, commit `feat(qpkg): LLVM toolchain injection and logged gcc rewrites`.

### Task 10: Build runner

**Files:**
- Create: `qpkg/src/runner.rs`

**Interfaces:**
- Consumes: `Pkgbuild`, `toolchain::build_env`.
- Produces: `pub fn run_functions(pb: &Pkgbuild, pkgbuild_path: &Path, workdir: &Path, env: &[(String,String)]) -> Result<()>` — for each of `prepare`,`build`,`package` present in `pb.functions`: `bash --noprofile --norc -c 'set -e; source "$0"; cd "$srcdir"; <fn>' <pkgbuild_path>` with `srcdir`/`pkgdir`/`pkgname`/`pkgver`/`pkgrel` exported, stdout+stderr appended to `workdir/build.log`; failure → `Error::Build { stage, log }`. Then `pub fn check_containment(pkgdir: &Path) -> Result<()>`: walk `pkg/`, fail on any symlink whose target escapes `pkgdir` and on any `..` path component → `Error::Containment(path)`.

- [ ] Tests (fixture PKGBUILDs written inline by the test): a package() that writes `$pkgdir/usr/bin/hello` succeeds and the file exists; stage order is observable (prepare appends to a log file that build reads); negative: a failing build() yields `Error::Build { stage: "build", .. }` and `build.log` contains the error text; absolute-symlink escape (`ln -s /etc/passwd $pkgdir/x`) fails containment; relative escape (`ln -s ../../../etc $pkgdir/x`) fails; an internal relative symlink (`ln -s ./real $pkgdir/alias`) passes.
- [ ] Run, pass, commit `feat(qpkg): run PKGBUILD functions in a scrubbed bash and contain the output`.

### Task 11: Artifact — .PKGINFO + deterministic tar.zst

**Files:**
- Create: `qpkg/src/artifact.rs`

**Interfaces:**
- Consumes: `Pkgbuild`.
- Produces: `pub fn package(pb: &Pkgbuild, pkgdir: &Path, out_dir: &Path) -> Result<PathBuf>` — generates `.PKGINFO` (`pkgname`, `pkgver = full_version()`, `arch = x86_64`, `size` = summed file bytes, `depend = …` per depends entry, `builddate = 0` for determinism, `packager = qpkg`), then writes `<name>-<fullver>-x86_64.pkg.tar.zst`: entries sorted by path, uid/gid 0, uname/gname root, mtime 0, `.PKGINFO` first.

- [ ] Tests: build a small `pkg/` tree, package it twice → byte-identical archives (determinism); decompress+read back → `.PKGINFO` is the first entry and contains `pkgver` with epoch when `epoch` is set; file content round-trips; negative: ownership in the archive is 0/0 even though the on-disk files are the test user's; an empty `pkg/` is refused with `Error::Build { stage: "package", .. }`.
- [ ] Run, pass, commit `feat(qpkg): emit deterministic Arch-format package archives`.

### Task 12: build + update commands, end-to-end wiring

**Files:**
- Create: `qpkg/src/build.rs`
- Modify: `qpkg/src/commands.rs`, `qpkg/src/main.rs`, `qpkg/src/sync.rs` (PKGBUILD fetch URLs)

**Interfaces:**
- Consumes: everything above.
- Produces: `build::run(index, name, no_rewrite, fetch) -> Result<PathBuf>`: look up record → fetch PKGBUILD (official: `https://gitlab.archlinux.org/archlinux/packaging/packages/<name>/-/raw/<version-tag>/PKGBUILD` with the `-` → `-` tag mangling documented in code; AUR: snapshot tarball) → `toolchain::check_host` → rewrite (unless `no_rewrite`) → `extract` → `check_arch` → `sources::stage` → `run_functions` → `check_containment` → `artifact::package` → `index.record_built`. `update::run(index, do_build, fetch)`: sync, then for each `all_built()` where `vercmp(upstream, built) == Greater` report (and optionally rebuild).

- [ ] Tests (fetch stubbed; a self-contained PKGBUILD with no sources and a pure-shell package()): full `build::run` produces an artifact and a `built` row whose `pkgbuild_sha256` matches the fetched bytes; `update` on a stale built version lists it and on a current one lists nothing (negative); a failed build leaves **no** `built` row (negative — the spec's "never claim success" rule); `--no-rewrite` skips the rewrite pass (assert via a PKGBUILD whose build() would only fail if `gcc` were rewritten to a missing tool — or simpler: assert the rewrite log is empty).
- [ ] Wire all subcommands in `main.rs`; remove the Task-1 stub errors.
- [ ] `#[cfg(feature = "online")]` end-to-end: `qpkg build zlib` against the live mirror (not in CI).
- [ ] Run `cargo xtask test` (full suite), commit `feat(qpkg): build and update commands complete the pipeline`.

---

## Self-review notes

- Spec coverage: §1→T1,6,12; §2→T3-6; §3→T9,12; §4→T7,8,10; §5→T11; §6→every task's negative cases. Staleness warning lands in T6; zsh flagship chain is an online-gated usage, not a CI test — deliberate, network builds can't run in CI.
- Type names are consistent (`PackageRecord`, `BuiltRecord`, `Pkgbuild`, `Repo`) across tasks.
- No placeholders; where a rule was uncertain (vercmp `1.01` vs `1.1`) the expected behaviour is stated in the test list itself.

## Execution Deviations

- **D1 — worktrees cannot live inside the repository.** The planned execution
  worktree at `.worktrees/qpkg` broke the kernel test suite before a line of
  qpkg existed: cargo merges every `.cargo/config.toml` on the walk up from
  the working directory and *joins list values*, so the kernel target's
  `runner` was concatenated with itself and invoked with its own command line
  as the ELF path. Moved to `../qunix-worktrees/qpkg`; gotcha recorded in
  CLAUDE.md.
- **D2 — the plan's vercmp expectation `1.0 < 1.0a` was wrong.** vercmp(8)
  documents `1.0a < … < 1.0rc < 1.0 < 1.0.a < 1.0.1`: an *attached* alpha
  trailer ages a version. Implemented and tested per the man page.
- **D3 — `PackageRecord` gained `package_base`** (Task 5, not planned):
  split packages fetch their PKGBUILD by base, not name, in both the AUR
  snapshot and the official GitLab layout; the repo-db parser reads `%BASE%`
  for the same reason.
- **D4 — `xz2` added** (Task 8): real-world sources are overwhelmingly
  `.tar.xz`; the host is Arch, so liblzma is a given.
- **D5 — GitLab project-name mangling is more than `+`→`plus`** (found by a
  live build of `tree`): names on GitLab's reserved-route list carry a
  `unix-` prefix (`tree` → `unix-tree`), and a missed name 302s to the
  sign-in page as 200 HTML — now sniffed and refused with a message naming
  the cause rather than surfacing as a bash syntax error.
- **D6 — live validation.** `qpkg sync` indexed 15,191 official + 117,309
  AUR packages from production data; `qpkg build tree` fetched, rewrote,
  cross-compiled and packaged a runnable statically-linked musl x86-64
  binary end to end. `zlib` correctly *refused*: GitHub serves different
  bytes for a generated commit-patch than the maintainer summed — the
  checksum path working as designed, not a qpkg defect.
