# Contributing to qunix

Two things are non-negotiable here: **every commit attests what assisted in
writing it**, and **branching follows git-flow**. Both are checked, not merely
requested — see [Enforcement](#enforcement).

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

## Before you open a PR

```sh
cargo xtask test
```

That must be green: 14 in-QEMU tests, 51 host tests, and the licensing check.

A PR is expected to state what it changes, why, and **what you verified rather
than assumed**. If you could not test something, say so — an honest gap is
reviewable, a silent one is not.

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

## Licensing

Contributions to qunix's own crates are `MIT OR Apache-2.0`. Contributions to the
Linux compatibility layer are `GPL-2.0`. The split is enforced at build time; see
[`LICENSING.md`](LICENSING.md). By submitting a change you agree it is licensed
under the zone its crate belongs to.

If you have worked from Linux kernel sources, say so in the PR. It affects which
zone the result belongs in, and that is much cheaper to settle before the code is
written than after.

## Enforcement

`cargo xtask attest` checks that every commit on your branch but not on
`develop` carries an `Assisted-by:` trailer, and that no work landed directly on
`main` or `develop`. It runs as part of `cargo xtask test`.

To check a different range explicitly:

```sh
cargo xtask attest origin/develop..HEAD
```

A requirement nobody checks is a suggestion, and this project has enough of those
already recorded as known limitations.
