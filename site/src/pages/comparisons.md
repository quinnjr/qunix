---
layout: ../layouts/Doc.astro
title: vs Linux / OpenBSD / Redox
kicker: Comparison
headline: How qunix differs from Linux, OpenBSD and Redox
tagline: >-
  qunix borrows from all three and diverges from each on specific, defensible points. This page
  states where and why, including the places where the established systems are simply right and
  qunix is making a research trade.
description: >-
  A detailed comparison of qunix against Linux, OpenBSD and Redox OS: kernel structure, memory
  allocators, language and safety strategy, scheduling, security posture and driver models, with
  the reasoning behind each divergence.
faq:
  - q: How is qunix different from Redox OS?
    a: >-
      Redox is a microkernel; qunix is monolithic. Redox moves drivers, filesystems and the
      network stack into userspace processes, which is a better engineering choice for
      reliability and a worse one for the question qunix is asking, because it relieves exactly
      the pressure on Rust that qunix exists to measure. Redox also ships a full userland and a
      real filesystem; qunix has neither.
  - q: How is qunix different from Linux?
    a: >-
      Linux is a production kernel written in C with decades of hardware support; qunix is a
      research kernel in Rust that runs one process. The substantive differences are in strategy:
      qunix bans shared mutable statics outright, treats every fallible allocation as returning an
      Option rather than panicking, keeps scheduling policy in a host-testable crate with no
      hardware dependency, and enforces a coverage ratchet and fuzzing model on its allocators.
  - q: How is qunix different from OpenBSD?
    a: >-
      OpenBSD's defining trait is opinionated simplicity in service of auditability, removing
      features to reduce attack surface. qunix shares the instinct that security properties should
      be demonstrated rather than asserted, but pursues it through machine-checked means: fuzzing
      against independent models, a coverage ratchet, and tests that assert the negative direction,
      rather than through review culture alone.
  - q: Is qunix a fork of any existing kernel?
    a: >-
      No. qunix is written from scratch. It borrows ideas, including the buddy allocator lineage
      from Unix and Linux, W^X and the demonstrate-rather-than-assert posture from OpenBSD, and the
      case for Rust in kernel space from Redox. It shares no code with any of them.
---

qunix learned from all three systems below. Where it diverges, the divergence is deliberate and
usually a research trade rather than a claim of superiority. Where the established systems are
simply right, this page says so.

## At a glance

| | qunix | Linux | OpenBSD | Redox |
| --- | --- | --- | --- | --- |
| Structure | Monolithic | Monolithic + modules | Monolithic | Microkernel |
| Language | Rust | C (+ Rust for drivers) | C | Rust |
| Maturity | Research; runs one process | Production, ~35 years | Production, ~30 years | Alpha, self-hosting |
| Drivers | None yet; DKMS-style planned | In-kernel + modules | In-kernel, static | Userspace processes |
| Physical allocator | Buddy, order 0–18 | Buddy + per-CPU page sets | Buddy-ish (`uvm`) | Buddy |
| Kernel heap | Segregated fit, 3/2 spacing | SLUB (object caches) | `pool(9)` | Slab |
| Scheduling | 3-band priority, one queue | EEVDF, per-CPU queues | Multilevel feedback | Round-robin |
| Isolation model | Address space + ring 3 | Address space + namespaces | Address space + `pledge`/`unveil` | Address space + capabilities |
| Verification | Fuzzing w/ models, coverage ratchet | Extensive; syzkaller | Audit culture, mitigations | Type safety, tests |

## Against Redox, the closest relative

Redox is the obvious comparison: a Rust operating system, actively developed, far more complete
than qunix. The divergence is structural.

**Redox is a microkernel. qunix is monolithic, on purpose.** Redox moves drivers, filesystems and
the network stack into userspace processes communicating over a scheme-based IPC layer. For
building a real operating system that is the better engineering choice, since a driver fault
becomes a restartable process instead of a panic.

For the question qunix is asking it is the wrong choice, because it relieves the pressure that
makes the question interesting. The hard parts of writing a kernel in Rust are hardware-aliased
page tables, interrupt handlers that preempt at arbitrary instruction boundaries, a thread stack
that has to be freed by a different thread, and per-CPU state reached through a segment base. Most
of those live in the parts a microkernel relocates to userspace, where ordinary Rust rules apply
again. A macrokernel keeps them in the kernel.

**What Redox does better, plainly:** it has a filesystem, a userland, a package ecosystem, and it
self-hosts. qunix runs one hand-assembled program that prints a string and exits. Redox has also
solved problems qunix has not reached, notably a coherent driver interface.

**Where qunix diverges on method:** it treats the allocators as the primary risk and fuzzes them
against an independent model asserting that no two live allocations overlap. That follows from a
specific observation, which is that these allocators do not fail loudly when they are wrong. Type
safety offers little help, because the relevant code sits inside the `unsafe` block implementing
the abstraction.

## Against Linux, structure in common and strategy apart

qunix is monolithic like Linux and borrows the buddy allocator lineage directly. The differences
are in policy, and each is a deliberate reaction to a known Linux pain point.

### Shared mutable state is banned, not managed

Linux uses per-CPU variables extensively but also carries a large amount of global mutable state
guarded by convention and lock discipline. qunix bans `static mut` outright as of its second
milestone. Where a static genuinely cannot be allocated, as with the bootstrap CPU's descriptor
tables which have to exist before the allocator does, it is an `UnsafeCell` in a `Sync` newtype
with a comment naming the single writer.

This is cheap in a young kernel and would be expensive to retrofit into an old one. It is not a
criticism of Linux; it is a thing you can only do at the start.

### Allocation failure is a value, not a panic

`frames::alloc` returns `Option<u64>`. Address-space creation returns `Option<VmSpace>`. Running
out of memory while creating a process is an ordinary failure the caller must handle, not a kernel
bug. Rust's `Option` makes this nearly free to express and impossible to ignore, which is a real
case where the language earns its place.

### Scheduling policy has no hardware dependency

Linux's scheduler is deeply entangled with per-CPU runqueue structures, and testing it means
booting. qunix keeps policy in a separate crate with no notion of a CPU, a stack or a context
switch, where threads are opaque IDs. It is host-tested, so a policy bug is a failed assertion in
a second rather than a machine that stops responding.

The cost is real and worth stating: the split means the policy cannot make decisions informed by
cache topology or NUMA distance without that information being passed in explicitly. Linux's
entanglement buys it something.

### Where Linux is simply right

Linux's per-CPU page allocator caches, its RCU machinery, and its enormous hardware support are
not things qunix is improving on. qunix has one global run queue and one lock, which is a
correctness-first placeholder that will not survive contact with a real workload.

## Against OpenBSD, same instinct and different instrument

OpenBSD's defining trait is aggressive simplicity in service of auditability: remove features,
reduce attack surface, make the code small enough that humans can actually read all of it. qunix
shares the underlying instinct, that security properties should be demonstrated rather than
asserted, and pursues it differently.

### Verification by machine rather than by reading

OpenBSD's principal instrument is a code review culture with unusually high standards. qunix leans
on machine-checked properties instead:

- **Fuzzing against an independent model.** The allocator targets do not look for panics. They
  maintain a separate model of what is live and assert, after every operation, that no two live
  allocations overlap.
- **A coverage ratchet.** Per-crate line coverage cannot fall without a written justification in
  the file itself; the tooling refuses to lower a floor that has no recorded reason.
- **Tests that assert the negative direction.** Not that coalescing merges, but that it *refuses*
  a live neighbour. Defects have twice occupied precisely that gap.

This is not better than OpenBSD's approach, which has three decades of evidence behind it where
qunix has none. It is a different bet: that a small project with one contributor gets more safety
per hour from machine-checked invariants than from review alone.

### Shared posture: W^X, and no exceptions

qunix maps process text executable and not writable, and stacks writable and never executable,
from the very first program it runs. That is OpenBSD's position, adopted deliberately: making W^X
retroactive is much harder than making it foundational.

### Where OpenBSD is simply right

`pledge` and `unveil` are the best ergonomics-to-security ratio in any production kernel. qunix
has nothing comparable and no syscall filtering at all. Its mitigations, including `malloc` guard
pages, kernel address randomisation and trapsleds, are production-tested in a way nothing here
is.

## Current limits

Stated plainly, because a research kernel that overstates its position is not useful to anyone:

- **No fault recovery.** A system call given a well-formed but unmapped user address will fault,
  and nothing catches it. Validation covers the higher half and wrapping, not mapping.
- **No syscall filtering, capabilities or namespaces.** Four calls, all unrestricted.
- **No ASLR, and no stack guard pages.** The bootloader's stack has none, so overflow writes
  through adjacent memory rather than faulting.
- **One user process.** Isolation between two address spaces is demonstrated by a test rather than
  by a running workload.
- **The allocators are the most-scrutinised part of the kernel for a reason.** Both have carried
  defects that produced no symptom until a fuzzing model asserted the invariant directly; both are
  fixed and carry regression tests.
