---
layout: ../layouts/Doc.astro
title: AI-assisted development
kicker: Findings
headline: What AI assistance is and is not good for in kernel development
tagline: >-
  qunix was built with substantial AI assistance, and the repository kept notes on what that
  produced. The useful finding is about which parts of the work benefit and which need a
  different kind of check.
description: >-
  Observations from building a Rust kernel with AI assistance: where it accelerated the work, the
  categories of defect that need machine-checked verification rather than review, and the
  practices that proved effective.
faq:
  - q: Can AI assistance help write an operating system kernel?
    a: >-
      Substantially, for parts that are well documented and individually understood, such as
      allocator structure, descriptor table setup, context switching and the SYSCALL entry path.
      What needs a different kind of check is whether a given piece of validation is complete,
      because in kernel code an incomplete check usually produces no visible symptom.
  - q: What kind of verification does AI-assisted systems code need?
    a: >-
      Checks that do not rest on a judgement about whether the code looks correct. In practice that
      meant fuzzing against an independently written model of the invariant, verifying each test by
      deliberately breaking the implementation, and process gates such as a coverage ratchet. Code
      review caught style and structure well and incomplete validation poorly.
  - q: Does Rust make AI-assisted kernel development safer?
    a: >-
      In a specific way. The defects that occurred were inside unsafe blocks, which is where a
      kernel's hard problems live regardless of who writes them. What Rust provided was locality.
      When the fuzzer reported an invariant violation, the search space was a handful of functions
      rather than the whole kernel.
  - q: What did AI assistance accelerate most?
    a: >-
      Breadth and recall. A buddy allocator, slab heap, context switch, descriptor table setup and
      SYSCALL stub in one project is a large amount of well understood code. Details like which MSR
      enables SYSCALL, or how SYSRET derives its segment selectors, are exactly the facts that
      otherwise cost an afternoon each.
---

qunix was built with substantial AI assistance, and the repository kept notes on what that
produced. What follows comes from that history rather than from impressions.

The short version: assistance was most valuable for breadth and recall, and least reliable at
judging whether a piece of validation was complete. The second category matters more in kernel
work than elsewhere, because an incomplete check there usually produces no visible symptom.

## Where it helped

**Mechanical breadth.** A buddy allocator, a slab heap, a context switch, descriptor table setup
and a `SYSCALL` entry stub add up to a lot of well documented code. It came together quickly and
mostly correctly.

**Architectural recall.** Which MSR enables `SYSCALL`. That `SYSRET` derives user CS from
`STAR[63:48] + 16`, which then dictates GDT ordering. That a busy TSS descriptor makes `ltr` raise
`#GP`. These are the details that otherwise cost an afternoon in the manuals each.

**Documentation density.** The kernel's comments record why rather than restating the code, at a
consistency most single-contributor projects do not sustain. This site exists because that
material was already written.

**Review at scale.** Running several specialised reviewers over one change surfaced real findings,
including in code that had already been verified once.

## Where it needs a different kind of check

### Validation that is correct but incomplete

The recurring pattern was not incorrect code. It was code that handled the cases it considered,
with a reasonable argument for why those cases were sufficient.

The clearest example is the physical allocator's free path, which checks that an address belongs
to a region the allocator owns. An early version confirmed the address sat inside a region but not
that the whole block fit. The coalescing loop a few lines below applied exactly that test to the
neighbouring block. After that was corrected, a later review found the same shape one step over:
the guard validated extent but not alignment.

Neither version was careless. Both read as complete until someone asked about a case they did not
cover.

### Tests that share the code's assumptions

When one process writes an implementation and its tests, the tests tend to encode the
implementation's understanding rather than an independent one. Several here could not have failed:

- An assertion that a four-page allocation returns nothing, made against a three-page region. No
  three-page region can produce a four-page block whatever the code does.
- A test asserting arguments were forwarded to a function that never received them, because the
  call site passed an empty list.
- An alignment test that recomputed the function's arithmetic rather than calling it, so it held
  for any implementation at all.

These turned up later and were replaced with assertions that exercise the property.

### Verifying that an edit landed

One commit reported that a function was covered by new tests when the edit adding them had not
taken effect. The content was fine. What was missing was a check that the change applied before
reporting it. Confirming that is cheap and worth doing every time.

### Details whose symptoms appear elsewhere

Two are worth naming because the failure shows up far from the mistake.

The `SYSCALL` convention and System V's do not line up. Every argument shifts by one register and
the call number moves from `rax` to `rdi`. Handling only part of that meant the kernel dispatched
on the wrong register, so both of a test program's calls returned an error and execution ran past
the end of the program. Nothing faulted where the mistake was.

The per-CPU `kernel_rsp` and `TSS.rsp0` serve different entry paths. Setting only the first
produces a kernel that handles system calls correctly and then fails on the first timer interrupt
taken while a process is running.

## What proved effective

The three practices that caught the most share one property. None of them depend on a judgement
about whether the code looks right.

### Fuzzing against an independently written model

The allocator targets do not look for crashes, since these allocators do not crash when they are
wrong. They keep a separate model of what is live and assert, after every operation, that no two
live allocations overlap and that every allocation sits inside a region that was actually added.

One caveat: several early findings were defects in the model rather than in the allocator. Writing
the model is most of the work, and a wrong model looks a lot like a real bug until you read both.

It also has a blind spot. The alignment gap above was unreachable by the fuzzer, because the
harness derived addresses by rounding to a block-size multiple. A target that normalises its
inputs cannot exercise the normalisation.

### Verifying each test by breaking the implementation

Every significant claim was checked by deliberately breaking the code and confirming the test
fails:

| Property | How it was checked |
| --- | --- |
| Preemption takes the CPU from a non-yielding thread | Disable preemption, the test times out |
| Exited threads are reclaimed | Disable reclamation, six live threads against an expected two |
| A new page table must carry the kernel half | Remove the copy, the machine faults on `mov cr3` |
| The context switch preserves callee-saved registers | Clobber one, the assertion fails |
| `free` rejects misaligned blocks | Remove the check, a later allocation overlaps a live block |

Slow, and the highest-value item here.

### Process gates that do not rely on memory

A coverage ratchet that will not lower a floor without a recorded reason in the file itself. A
check that every commit records what assisted in writing it. A licensing check that fails the
build if a compatibility-layer crate inherits the wrong licence. None of these depend on anyone
remembering to look.

## Practical conclusions

Use assistance for breadth and verify completeness separately. Producing a well structured
allocator is work models do well. Establishing that a validation path covers every case is better
served by a property expressed independently of the implementation.

Treat tests written alongside the code as provisional. They are useful, and they are not
independent evidence. A fuzzing model or a deliberately broken build is.

Confirm that changes landed before reporting them. The alternative is a claim that propagates into
other decisions.

Rust contributes locality rather than immunity. The defects here lived inside `unsafe` blocks,
which is where a kernel's hard problems live regardless of authorship. What the language gave was
a bounded search space when something went wrong. That is a real benefit and a narrower one than
the usual framing suggests.
