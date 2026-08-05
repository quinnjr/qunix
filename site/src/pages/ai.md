---
layout: ../layouts/Doc.astro
title: AI-assisted development
kicker: Findings
headline: What AI assistance is and is not good for in kernel development
tagline: >-
  qunix was written with heavy AI assistance and keeps a record of exactly what that produced.
  This page is the honest accounting: the failure mode is not incompetence, it is plausibility —
  code that compiles, passes, reviews well, and is wrong.
description: >-
  An evidence-based account of AI-assisted kernel development: where large language models helped,
  the specific bugs they produced, why those bugs survived review, and the practices that caught
  them.
faq:
  - q: Can AI write an operating system kernel?
    a: >-
      It can write code that boots, allocates, schedules and reaches userspace — qunix does all of
      that. What it cannot do reliably is know when it is wrong. Every memory-corruption bug in
      this project was produced by a model, accompanied by a confident comment explaining why the
      code was sound, and passed both review and the test suite. The output is competent; the
      self-assessment is not.
  - q: What is the main risk of using AI to write systems code?
    a: >-
      Plausibility. A model produces code that compiles, matches surrounding idiom, carries a
      well-written safety comment and passes the tests it also wrote. In a kernel the failure mode
      is silent — an allocator that hands out the same frame twice returns success on every
      operation — so nothing surfaces the error. The mitigation is machine-checked invariants that
      do not depend on the model's judgement.
  - q: Did Rust make AI-assisted kernel development safer?
    a: >-
      Partially, and not in the advertised way. Rust did not prevent the bugs; every one lived
      inside an unsafe block. What it did was make them local — the search space for a
      memory-corruption bug was a handful of functions rather than the whole kernel. That is a
      genuine and substantial benefit, but it is containment, not prevention.
  - q: What practices actually caught AI-introduced kernel bugs?
    a: >-
      Fuzzing with an independent model of correctness, tests that assert the negative direction,
      verifying each test by deliberately breaking the code, and adversarial multi-agent review.
      What did not work: reading the diff, trusting safety comments, and a green test suite.
---

qunix was written with heavy AI assistance. Rather than treating that as an embarrassment or a
selling point, the project records what it produced. This page is the accounting.

The headline finding is narrow and specific:

> The failure mode of AI-assisted systems programming is not incompetence. It is **plausibility**
> — code that compiles, matches the surrounding idiom, carries a confident and well-written safety
> comment, passes the tests it also wrote, and is wrong.

## What worked well

Being fair before being critical, because the assistance was genuinely productive:

- **Mechanical breadth.** Writing a buddy allocator, a slab heap, a context switch, GDT/IDT setup
  and a `SYSCALL` stub in one project is a lot of well-documented, individually-understood code.
  A model produces that quickly and largely correctly.
- **Documentation density.** qunix's comments explain *why* rather than restating the code, at a
  density most solo projects never reach. The record on this site exists because that discipline
  was cheap to sustain.
- **Recall of architectural detail.** Which MSR enables `SYSCALL`, that `SYSRET` derives user CS
  from `STAR[63:48]+16`, that a busy TSS descriptor makes `ltr` raise `#GP` — these are exactly
  the facts that cost a human an afternoon in the SDM each.
- **Adversarial review at scale.** Running seven specialised reviewers over one diff found real
  defects, including one the author had already verified and shipped.

## What failed, with evidence

### The bug class: correct-looking code that is silently wrong

**Two allocator bugs that handed out the same memory twice.** Neither crashed. Every operation
returned success. Both lived inside `unsafe` blocks that carried comments explaining why they were
sound; the comments were wrong. Both survived a review pass.

The second one is the more instructive. After a fix added an extent check to `free`, a later
review found the identical hole one step over — the guard validated extent but not alignment. The
first fix was correct, well-commented, tested, and incomplete in exactly the way the original bug
had been.

### The self-reinforcing failure: tests that cannot fail

A model writing both the code and its tests will write tests that pass. Several in this project
could not have failed:

- An assertion that `alloc(2)` returns `None` against a three-page region. No three-page region
  can yield a four-page block regardless of what the code does. It was labelled "the consequence
  that matters".
- A test asserting arguments are forwarded to a function that never received them, because the
  dispatch site passed an empty slice. The parameter's only non-empty caller was its own test.
- An alignment test that recomputed the function's arithmetic instead of calling it. It passed for
  any implementation, including none.

### The claim that was simply false

A commit stated a function was "covered by 3 new tests". The tests were never inserted — the edit
targeted an anchor that did not exist, silently did nothing, and the count was reported without
re-checking. A coverage floor was then lowered partly on that claim.

This is the most important item on the page. The model did not hallucinate the tests' *content*;
it failed to verify that its own edit landed, and then reported success. **The error was in
verification, not generation.**

### Non-obvious mistakes with silent consequences

- **Register convention.** The `SYSCALL` stub moved `r10` into `rcx` and assumed the rest lined
  up. They do not — every argument shifts by one register and the number moves from `rax` to
  `rdi`. The kernel dispatched on whatever was in `rdi`, so a process calling `exit(7)` had its
  message *address* interpreted as the syscall number. Both syscalls returned an error and the
  program ran off its end into a `ud2`. Nothing faulted where the mistake was.
- **A second stack pointer.** `TSS.rsp0` and the per-CPU `kernel_rsp` are different mechanisms for
  different entry paths. Setting only one produced a kernel that serviced syscalls perfectly and
  died on the first timer tick during userspace.
- **A test that hung every later test.** A spinner thread with no termination condition was
  inherited by the next test in the shared kernel, which then hung until the harness timeout.

## Why review did not catch these

Seven independent reviewers over one diff missed the alignment hole until specifically pointed at
the free path — and the two original allocator bugs survived a full review pass.

The reason is structural. Review reads for *plausibility*, and this code is maximally plausible:
consistent naming, correct idiom, safety comments that cite the right architectural facts. A human
reviewing a `free` function that checks the region containing an address, and has a well-argued
comment about why that check is sufficient, has no signal that the check is incomplete.

The three things that did work all share one property: **they do not depend on anyone's judgement
about whether the code looks right.**

### 1. Fuzzing against an independent model

The allocator fuzz targets do not look for panics. They maintain a separate model of what is live
and assert, after every operation, that no two live allocations overlap and every allocation lies
inside a region that was really added. That assertion is what found the extent bug.

There is a trap here, and it cost three false alarms: three of the first four "crashes" were
harness defects, not allocator defects. A wrong model is indistinguishable from a bug until you
read the code. Writing the model is where the work is.

### 2. Verifying every test by breaking the code

Each claim on this site was checked by deliberately breaking the implementation and confirming the
test fails:

| Property | How it was verified |
| --- | --- |
| Preemption takes the CPU from a spinner | Disabled `preempt`; test hangs to timeout |
| Exited threads are reaped | Disabled reclamation; 6 live threads vs expected 2 |
| The kernel half must be in a new page table | Removed the copy; machine triple-faults on `mov cr3` |
| The context switch preserves `r12` | Made `switch` zero it; assertion fails |
| `free` rejects misaligned blocks | Removed the check; `alloc` returns an overlapping block |

This is slow and it is the single highest-value practice on the list.

### 3. Machine-checked process invariants

A coverage ratchet that refuses to lower a floor without a written reason in the file. An
attestation check that every commit names what assisted in writing it. A licensing check that
fails the build if a compatibility-layer crate inherits the wrong licence. None of these depend on
anyone remembering.

## What this suggests about AI in kernel work

**Use it for breadth, not for judgement.** Producing a correct-looking buddy allocator is a task
models are good at. Deciding whether the free path's validation is complete is a task they are
bad at, and — more importantly — a task at which they cannot assess their own reliability.

**Assume the tests are complicit.** If the same process writes the code and the tests, the tests
encode the same misunderstanding. The only reliable check is a property expressed independently of
the implementation, which is what a fuzzing model is.

**Verify that edits landed.** The false coverage claim was not a reasoning failure. It was a
failure to confirm that an action had the effect it reported. That is cheap to check and
catastrophic to skip.

**Rust helps, in a specific and limited way.** It did not prevent a single one of these bugs — all
of them lived inside `unsafe`. What it did was bound where they could be. When the fuzzer reported
overlapping allocations, the search space was four functions. In C it would have been the kernel.

That containment is worth a great deal. It is just not the claim usually made for it.
