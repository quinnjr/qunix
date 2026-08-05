---
layout: ../layouts/Doc.astro
title: Overview
kicker: Educational research kernel
headline: How far does Rust get you when you write a monolithic kernel?
tagline: >-
  qunix is an x86_64 macrokernel written from scratch in Rust. It exists to answer two questions
  honestly: where does Rust stop helping when the abstractions it protects you with are the very
  things you are implementing, and where does AI assistance stop helping when a plausible,
  compiling, passing answer can still be wrong in a way nothing reports.
description: >-
  An educational x86_64 macrokernel in Rust, built to find where the language stops helping and
  where AI-assisted development stops helping.
faq:
  - q: What is qunix?
    a: >-
      qunix is an educational monolithic (macrokernel) operating system kernel for x86_64, written
      from scratch in Rust. It boots via UEFI, runs preemptive kernel threads across isolated
      address spaces, brings up every processor core, and enters ring 3 through its own
      SYSCALL/SYSRET ABI. It is a research project, not production software.
  - q: Is Rust a good language for writing an operating system kernel?
    a: >-
      Rust helps, but not in the way it is usually advertised. In qunix it did not prevent a single
      memory-corruption bug — every one lived inside an `unsafe` block with a comment explaining
      why it was sound. What it did was make those bugs local: the search space when the fuzzer
      reported overlapping allocations was four functions rather than the entire kernel. That
      containment is valuable, but it is containment rather than prevention.
  - q: Can a large language model write a working kernel?
    a: >-
      It can write one that boots, allocates, schedules and reaches userspace, which qunix does.
      What it cannot do reliably is recognise when it is wrong. Every memory-corruption bug in this
      project was model-written, carried a confident safety comment, and passed both review and the
      test suite. The generation is competent; the self-assessment is not.
  - q: Is qunix ready to use?
    a: >-
      No. It has no filesystem, no driver model, no network stack and no stable ABI, and its
      application processors come online and park rather than running threads. It runs one
      hand-assembled userspace program. Nothing here should run anything you care about.
---

qunix is not trying to be a better Linux. It boots UEFI, runs preemptive threads across isolated
address spaces, brings up every core, and drops to ring 3 through its own syscall ABI — and every
one of those steps produced a specific, recorded lesson about the two questions above.

This site is the write-up. The [design decisions](/qunix/design/) page covers what the kernel does
and why; [vs Linux / OpenBSD / Redox](/qunix/comparisons/) covers where it deliberately diverges
from the systems it learned from; and [AI-assisted development](/qunix/ai/) is the honest account
of what a model got right, what it got wrong, and what kind of wrong it was.

## What exists today

| Subsystem | State |
| --- | --- |
| Boot | UEFI via Limine v11, SHA-256-pinned bootloader, custom target spec |
| Toolchain | clang + LLD end to end; no `gcc`, no GNU `ld` script, musl for host tests |
| Physical memory | Buddy allocator, order 0–18, intrusive free lists, 128 regions |
| Kernel heap | Segregated-fit slab with 17 size classes at 3/2 spacing, large-block recycling |
| Per-CPU state | GDT, TSS, IDT and fault stack per core, reached through `GS` |
| Scheduling | Three-band priority run queue, cooperative yield, APIC-timer preemption |
| SMP | All application processors online and parked |
| Address spaces | Owned page tables, kernel half shared by reference, full teardown |
| Userspace | `SYSCALL`/`SYSRET`, W^X, ring 3, four syscalls |
| Verification | 35 in-QEMU tests, 115 host tests, two fuzz targets, coverage ratchet |

Not present: a filesystem, a driver model, a network stack, a stable ABI, or any claim to
production readiness. Application processors come online and then park — they do not run threads
yet, for a reason given in the [design notes](/qunix/design/#smp).

## The honest position on Rust

Rust did not prevent this project's worst bugs. It made them **local**.

Every memory-corruption defect qunix has shipped lived inside a block that was already marked
`unsafe` and already carried a comment explaining why it was sound. The comment was wrong. What
the language bought was that the search space for "where could this possibly be" was four
functions rather than four hundred.

That is a real benefit and it is worth being precise about, because the marketing claim — that
Rust prevents memory-safety bugs — is not the claim that survives contact with a kernel. A kernel
is code that *creates* the notion of ownership the borrow checker reasons about. Page tables are
aliased by hardware. Interrupt handlers preempt at arbitrary instruction boundaries. The stack a
thread stands on is an object somebody else has to free. None of that is expressible in safe Rust.

The interesting question is therefore not "is Rust memory-safe here" — it is not, in the parts
that matter — but **how small can the unsafe surface be made, and how much does shrinking it
actually buy you**. qunix's answer so far: 20 public `unsafe` functions, and the buying is real
but narrower than advertised.

## Three defects worth the whole project

**The allocator that hands out the same frame twice.** It happened twice. Neither time did
anything crash, panic, or return an error — every operation succeeded and reported success.
Fuzzing found it only once the harness stopped looking for panics and started asserting that no
two live allocations overlap.

**A fuzz target that could not find its own bug class.** After an extent check was added to
`free`, a review found the same hole one step over: the guard validated extent but not alignment.
The fuzzer could never have caught it, because the harness derived addresses by rounding down to a
block-size multiple — a misaligned free was unreachable by construction. *A target that
normalises its inputs cannot find bugs in the normalisation.*

**A test that measured test order rather than the allocator.** An assertion that a 16 KiB
allocation advances the bump region held only because nothing had freed a 16 KiB block first.
Thread stacks are exactly 16 KiB. The moment a scheduler existed, the assertion began reporting a
bug that was not there.

## Method

Every claim on this site was checked by breaking the code and watching a test fail. That is a
project rule, not a flourish:

> When you add a test, ask what it would take for it to fail. If the answer is "nothing short of
> deleting the function", it is not a test yet.

Concretely: preemption was verified by disabling `preempt` and confirming the spinner test hangs
to timeout; thread reaping by disabling reclamation and confirming 6 live threads against an
expected 2; the shared kernel page-table half by removing the copy and confirming the machine
triple-faults on `mov cr3`; and the context switch by making it clobber `r12` and confirming the
register assertion fails.
