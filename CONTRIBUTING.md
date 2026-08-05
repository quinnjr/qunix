# Contributing to qunix

Three things are non-negotiable here:

- **Every commit attests what assisted in writing it.**
- **Branching follows git-flow.**
- **You are responsible for the pull request you open, and nothing is accepted
  unverified.**

The first two are checked by `cargo xtask test` — see
[Enforcement](#enforcement). The third cannot be, which is exactly why it is
stated first among equals rather than left implied.

Everything else is in [`CLAUDE.md`](CLAUDE.md), which records the conventions and
the traps this codebase has already paid for. Read it before your first change.

## LLM attestation

Every commit carries an `Assisted-by:` trailer naming what helped write it, or
`none`.

```
Assisted-by: Claude Opus 5
Assisted-by: GitHub Copilot
Assisted-by: none
```

Multiple tools get multiple trailers. Attest the tools that produced or
substantially shaped the change — not your editor's autocomplete, and not a
model you asked an unrelated question.

### Why a separate trailer

This repository already used `Co-Authored-By:` for the assistant that wrote most
of M0, and GitHub renders that as co-authorship. That is the wrong shape for an
attestation policy for two reasons: a model is not an author holding rights that
`Co-Authored-By:` implies, and — more practically — the field cannot express the
negative. `Co-Authored-By: none` is nonsense, so the absence of the trailer is
ambiguous between "wrote it myself" and "forgot".

`Assisted-by:` requires an explicit value. Silence is a failure, not an implied
`none`. `Co-Authored-By:` keeps its ordinary meaning for human co-authors.

### What attestation is not

It is not a disclaimer, and it does not lower the bar. **You are the author of
what you submit.** If a model wrote it, you are still accountable for it being
correct, and for having understood it well enough to defend it in review.

That is not boilerplate here. Three review passes over M0 each found real defects
in the previous pass's output, including two memory-corruption bugs that survived
a seven-agent review because the accompanying tests only asserted the happy path.
Assisted code in this repository has a demonstrated failure mode: it is plausible,
it compiles, its tests pass, and it is wrong. Attestation exists so reviewers know
where to apply that scepticism.

## Branching — git-flow

| Branch | From | Into | For |
| --- | --- | --- | --- |
| `main` | — | — | released, known-good |
| `develop` | — | — | integration; the default branch |
| `feature/<name>` | `develop` | `develop` | all ordinary work |
| `release/<version>` | `develop` | `main` + `develop` | stabilising a milestone |
| `hotfix/<name>` | `main` | `main` + `develop` | urgent fixes to a release |

Never commit directly to `main` or `develop`.

```sh
git switch develop && git pull
git switch -c feature/m1-scheduler
# ... work, commit ...
git push -u origin feature/m1-scheduler
gh pr create --base develop
```

Name feature branches after the work, and where it maps to a milestone task, say
so: `feature/m1-context-switch`, `feature/m2-vfs-path-resolution`. Milestones and
their task breakdowns live in [`docs/superpowers/plans/`](docs/superpowers/plans/).

### Milestone-sized work

Larger changes get a spec, then a plan, then code:
`docs/superpowers/specs/` → `docs/superpowers/plans/` → `feature/*`.

Plans carry an **Execution Deviations** section. When reality contradicts the
plan — it did sixteen times in M0 — record it there rather than quietly
diverging. The plan is a hypothesis; the deviations are the result.

## Commits

[Conventional Commits](https://www.conventionalcommits.org/): `feat`, `fix`,
`docs`, `test`, `build`, `refactor`, `perf`, `chore`, with an optional scope —
`feat(mm): …`, `fix(hal): …`.

Explain **why** in the body. The subject says what changed; the body says what
was wrong with the previous state. A commit that says only what it did is a
diff with extra steps.

## Responsibility

**A pull request is the responsibility of the person who opens it.** Not the
tool that helped write it, not the reviewer who approves it, and not the
maintainer who merges it. If you open it, you own it: its correctness, its
tests, and its consequences.

That holds whatever produced the code. A model wrote most of M0 — the standard
is unchanged by that fact. Opening a PR is a claim that you have read every line
in it and can defend it. If you cannot explain why a hunk is there, it is not
ready to submit; delete it or go and understand it.

The obligation does not transfer on merge. If a change of yours turns out to be
wrong, the expectation is that you are the one who fixes it.

## Verification before acceptance

**No PR is accepted without being verified.** Verification means someone ran it
and checked the claims — not that it looked reasonable, and not that CI was
green, which only proves the assertions that exist actually pass.

Two independent obligations:

- **You verify before opening.** `cargo xtask test` green, the change exercised,
  and every claim in the PR description something you checked rather than
  expected.
- **A reviewer verifies before merging.** Independently, not by re-reading your
  description. A review that only reads the diff has not verified anything.

That distinction is load-bearing here. This codebase has shipped changes where
the full suite passed, the reasoning was sound, and the code was wrong —
including two memory-corruption bugs that survived a seven-agent review because
every test asserted the happy path. Green is a necessary condition, never a
sufficient one.

### Coverage must not regress

`cargo xtask coverage` measures line coverage per crate and fails if any crate
falls below the floor in `coverage-baseline.toml`. It runs on every pull request.

If your change lowers coverage, the fix is normally a test. There is one honest
exception: code that cannot be covered at all — `xtask` command dispatch that
only spawns cargo is the recurring case — legitimately moves a floor down. That
is a decision to argue for in the PR, naming which code is uncoverable and why,
not a quiet edit to make a red build green. Update the floor with:

```sh
cargo xtask coverage --update
```

Two limits worth knowing before you trust the number:

- **The kernel is not measured.** Its 14 tests run inside QEMU, where host
  instrumentation cannot reach. Coverage says nothing about `qunix-kernel`, so a
  change there can be entirely untested and the ratchet will not notice.
- **The floors differ enormously by crate, on purpose.** `qunix-mm` sits above
  94% because it is pure logic; `port.rs` is 0% because it is `in`/`out`
  instructions that cannot execute on the host, and `xtask` is low because it
  mostly shells out to cargo and QEMU. Ratcheting one aggregate figure would let
  untestable orchestration growth fail a well-tested allocator change, which
  teaches people to game the number instead of testing their code.

Coverage is a floor against carelessness, not evidence of correctness. Both
memory-corruption bugs this project has shipped were in code that was covered —
the tests executed the lines and asserted the wrong thing.

### What a PR must state

- What changes, and what was wrong with the previous state.
- **What you verified, and how.** Name the commands and what you observed.
- **What you did not verify.** An honest gap is reviewable; a silent one is a
  trap for whoever hits it later.

"Tests pass" is not verification of anything except that the tests pass. If a
change adds a guard, show the guard failing when it should. If it fixes a bug,
show the test failing before the fix.

## Before you open a PR

```sh
cargo xtask test
```

That must be green: 14 in-QEMU tests, 89 host tests, the licensing check, and the
attestation check. CI runs the same suite plus the coverage ratchet on every PR.

### Tests

New behaviour needs a test that can fail. Before adding one, ask what it would
take to make it fail; if the answer is "nothing short of deleting the function",
it is not a test yet.

Assert the **negative** direction, especially for anything that frees, unmaps, or
recycles. This codebase's worst bugs lived precisely where only the positive
direction was covered: that coalescing merges was asserted, that it must *refuse*
a live buddy was not.

Note the in-QEMU harness reports its verdict through a single port write, so
anything that degrades output without halting still passes green. A green run is
not evidence that output was correct.

### Fuzzing

`cargo xtask fuzz` runs libFuzzer over the buddy allocator and the slab heap
(`cargo install cargo-fuzz` first). CI runs 60 seconds per target on every PR,
which proves the harnesses build and nothing shallow regressed — not that the
allocators are correct.

If you change `qunix-mm`, run it longer than CI does before opening the PR:

```sh
cargo xtask fuzz buddy --seconds=600
```

The targets assert that **no two live allocations overlap**, because that is the
failure these allocators actually produce — they do not crash when they are
wrong. If you add a target, assert a property an independent model can check,
not merely that nothing panicked.

Be aware that a wrong model looks exactly like a bug. Three of the first four
crashes found here were harness defects, not allocator defects; see the Fuzzing
section of [`CLAUDE.md`](CLAUDE.md) for which, and why each was tempting.

## Licensing

Contributions to qunix's own crates are `MIT OR Apache-2.0`. Contributions to the
Linux compatibility layer are `GPL-2.0`. The split is enforced at build time; see
[`LICENSING.md`](LICENSING.md). By submitting a change you agree it is licensed
under the zone its crate belongs to.

If you have worked from Linux kernel sources, say so in the PR. It affects which
zone the result belongs in, and that is much cheaper to settle before the code is
written than after.

## Enforcement

`cargo xtask attest` checks that every non-merge commit on your branch but not
on `develop` carries an `Assisted-by:` trailer, and that no work landed directly on
`main` or `develop`. It runs as part of `cargo xtask test`.

To check a different range explicitly:

```sh
cargo xtask attest origin/develop..HEAD
```

A requirement nobody checks is a suggestion, and this project has enough of those
already recorded as known limitations.
