---
layout: ../layouts/Doc.astro
title: Overview
kicker: Educational research kernel
headline: How far does Rust get you when you write a monolithic kernel?
tagline: >-
  qunix is an x86_64 macrokernel written from scratch in Rust. It was built to answer two
  questions with evidence rather than opinion. Where does Rust stop helping once you are
  implementing the abstractions it normally protects you with? And which parts of kernel work
  actually benefit from AI assistance?
description: >-
  An educational x86_64 macrokernel in Rust, documenting where the language stops helping and
  which parts of kernel development benefit from AI assistance.
faq:
  - q: What is qunix?
    a: >-
      qunix is an educational monolithic operating system kernel for x86_64, written from scratch
      in Rust. It boots via UEFI, runs preemptive kernel threads across isolated address spaces,
      brings up every processor core, and enters ring 3 through its own SYSCALL ABI. It is a
      research project rather than production software.
  - q: Is Rust a good language for writing an operating system kernel?
    a: >-
      It helps, though more narrowly than the usual framing suggests. A kernel's hardest problems
      live inside unsafe blocks, where the borrow checker offers no guarantees. What Rust provides
      is locality. When an invariant check fails, the search space is a handful of functions rather
      than the whole kernel. That is containment rather than prevention, and it is still worth a
      lot.
  - q: Can AI assistance produce a working kernel?
    a: >-
      It helped produce one that boots, allocates, schedules and reaches userspace. The assistance
      was most useful for breadth and for recalling architectural detail. It was least reliable at
      judging whether a piece of validation was complete, which matters more in kernel code than
      elsewhere because an incomplete check there usually produces no visible symptom.
  - q: Is qunix ready to use?
    a: >-
      No. There is no filesystem, no driver model, no network stack and no stable ABI. Application
      processors come online and park rather than running threads. It runs one hand-assembled
      userspace program.
---

qunix is not an attempt to build a better Linux. It boots UEFI, runs preemptive threads across
isolated address spaces, brings up every core, and drops to ring 3 through its own syscall ABI.
Each of those steps produced something specific worth writing down.

This site is that write-up. [Design decisions](/qunix/design/) covers what the kernel does and the
constraint behind each choice. [vs Linux / OpenBSD / Redox](/qunix/comparisons/) covers where it
diverges from the systems it learned from. [AI-assisted development](/qunix/ai/) records which
parts of the work benefited and which needed a different kind of check.

## What exists today

| Subsystem | State |
| --- | --- |
| Boot | UEFI via Limine v11, SHA-256-pinned bootloader, custom target spec |
| Toolchain | clang and LLD end to end, no `gcc`, no GNU `ld` script, musl for host tests |
| Physical memory | Buddy allocator, order 0 to 18, intrusive free lists, 128 regions |
| Kernel heap | Segregated fit with 17 size classes at 3/2 spacing, large-block recycling |
| Per-CPU state | GDT, TSS, IDT and fault stack per core, reached through `GS` |
| Scheduling | Three-band priority run queue, cooperative yield, APIC-timer preemption |
| SMP | All application processors online and parked |
| Address spaces | Owned page tables, kernel half shared by reference, full teardown |
| Userspace | `SYSCALL`/`SYSRET`, W^X, ring 3, four syscalls |
| Verification | 35 in-QEMU tests, 115 host tests, two fuzz targets, coverage ratchet |

Missing: a filesystem, a driver model, a network stack, a stable ABI. Application processors come
online and then park. They do not run threads yet, for a reason given in the
[design notes](/qunix/design/#smp).

## What Rust actually contributes

A kernel is an awkward place to evaluate Rust, because it is the code that creates the notion of
ownership the borrow checker reasons about. Page tables are aliased by hardware. Interrupt
handlers preempt at arbitrary instruction boundaries. The stack a thread runs on has to be freed
by some other thread. None of that fits in safe Rust, and those are the parts of a kernel that are
actually hard.

So the useful measure is how small the unsafe surface can be made, and what shrinking it buys.
qunix currently exposes 20 public `unsafe` functions. Each carries a precondition a caller has to
establish, and writing those preconditions down caught several cases where the obligation could
not be discharged by anyone.

The concrete benefit showed up during fuzzing. When a model reported overlapping allocations, the
search space was a handful of functions instead of the whole kernel. That is containment rather
than prevention. It still saved days.

## Three findings that justified the approach

**Allocators fail quietly.** An allocator that hands the same frame to two callers returns success
on every operation. Nothing crashes and nothing returns an error. Fuzzing only surfaced this class
of defect once the harness stopped looking for crashes and started asserting that no two live
allocations overlap.

**A fuzz target can be blind to its own subject.** After an extent check was added to the free
path, a review found the same shape one step over: the guard validated extent but not alignment.
The fuzzer could not have reached it. Its harness derived addresses by rounding to a block-size
multiple, so misaligned input was unreachable by construction. A target that normalises its inputs
cannot exercise the normalisation.

**A test can measure something other than what it claims.** An assertion that a 16 KiB allocation
advances the bump region held only because nothing had previously freed a 16 KiB block. Thread
stacks are exactly 16 KiB. As soon as a scheduler existed, the assertion started reporting a
problem that did not exist.

## Method

Claims on this site were checked by breaking the implementation on purpose and confirming a test
fails. That follows from a rule the project keeps:

> When you add a test, ask what it would take for it to fail. If the answer is "nothing short of
> deleting the function", it is not a test yet.

In practice: preemption was checked by disabling it and confirming the spinner test runs to
timeout. Thread reclamation was checked by disabling it and counting six live threads against an
expected two. The shared kernel page-table half was checked by removing the copy and watching the
machine fault on `mov cr3`. The context switch was checked by making it clobber a callee-saved
register.
