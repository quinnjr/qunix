---
layout: ../layouts/Doc.astro
title: Design decisions
kicker: Architecture
headline: qunix design decisions, and the reasoning behind each one
tagline: >-
  Every non-obvious choice in the kernel, written down with the constraint that produced it. Where
  a decision was later proved wrong by a test or a review, that is recorded too.
description: >-
  The architecture of qunix, a Rust macrokernel for x86_64: buddy and slab allocators, per-CPU
  state through GS, SMP bring-up, preemptive scheduling, owned address spaces, and a native
  SYSCALL ABI — each with the reasoning and the failures behind it.
faq:
  - q: Why is qunix a monolithic kernel rather than a microkernel?
    a: >-
      Because the research question is about Rust's limits under pressure, and a microkernel
      relieves exactly the pressure that is interesting. The hard parts of kernel work in Rust —
      hardware-aliased page tables, interrupt reentrancy, a thread stack that must be freed by
      another thread — mostly live in the parts a microkernel moves to userspace. A macrokernel
      keeps them in Rust, which is the point.
  - q: Why does qunix use a buddy allocator with intrusive free lists?
    a: >-
      A buddy allocator gives contiguous physical allocations for DMA and huge pages, which a
      bitmap allocator cannot do cheaply. The free lists are threaded through the free frames
      themselves so the allocator needs no side table proportional to RAM; the cost is that
      metadata lives in memory the allocator hands out, which is why a free tag is validated
      before any coalescing decision trusts it.
  - q: Why does every CPU get its own GDT, TSS and IDT?
    a: >-
      The TSS holds rsp0 and the interrupt stack table, both of which are per-CPU by definition.
      Sharing one means two cores taking a fault switch to the same stack and overwrite each
      other's frames. qunix originally used static mut singletons, which were sound only while
      exactly one CPU existed; M1 replaced them with a per-CPU block reached through GS.
  - q: Why did qunix drop gcc and GNU ld entirely?
    a: >-
      The project targets LLVM, and mixing toolchains means two linker scripts, two sets of
      relocation semantics and two failure modes. The kernel's layout is expressed as LLD flags
      rather than a GNU linker script, and host tests build against musl. The only remaining C is
      one translation unit inside a fuzzing dependency, compiled with clang.
---

This page documents what qunix does and why. It is organised by subsystem, and each section states
the constraint first — the decision usually follows from it.

## Toolchain and boot

### Why there is no GNU toolchain anywhere

qunix builds with clang and LLD end to end. The kernel's memory layout is expressed as LLD flags
(`--image-base`, `--entry`) rather than a GNU linker script, and host-side tests build against
musl rather than glibc.

This is not aesthetic. Mixing LLVM and GNU tooling in a freestanding project means two sets of
relocation semantics, two linker script dialects, and failures that reproduce under one linker and
not the other. Committing to one removes a whole class of "works on my machine".

The one place C survives is a single translation unit inside a fuzzing dependency. It is compiled
with clang, and CI installs `clang` and `lld` rather than `build-essential`.

> A related trap, paid for once: `cargo-fuzz` defaults `--target` to the triple *it* was built
> for. A source-built copy picks gnu; the prebuilt binary CI downloads is musl-static, and a
> sanitizer cannot link into a static libc. The triple is now pinned explicitly.

### Why the bootloader binary is SHA-256 pinned

qunix boots via Limine, whose prebuilt binaries live on a mutable branch. The build pins a commit
*and* verifies the SHA-256 of `BOOTX64.EFI` on every run, not just after a fresh clone. A pinned
commit alone is trust-on-first-use: a checkout corrupted or swapped afterwards would be handed to
the guest firmware forever.

## Physical memory

### Why a buddy allocator

The kernel needs physically contiguous memory for DMA buffers and huge pages. A bitmap allocator
makes "find 2 MiB of contiguous free frames" a scan; a buddy allocator makes it a list pop.

`MAX_ORDER` is 18 — 1 GiB blocks. Capping at order 10 (4 MiB) would shard a large machine's memory
into blocks that can never merge: 64 GiB becomes 16,384 order-10 blocks, and 1 GiB huge pages
become impossible to satisfy. The cost of raising it is one `u64` of list head per order.

### Why free-list metadata lives inside free frames

Each free block carries three words in its first 24 bytes: next, prev, and a tag. That means no
side table proportional to RAM, and it makes `unlink` an O(1) operation instead of a list walk.

The cost is that metadata lives in memory the allocator hands out, which creates a specific
hazard: a caller who writes to a frame after freeing it, or who forges a tag, can steer the
allocator. So the tag mixes address and order with a constant, and coalescing validates it before
trusting a neighbour is free. A block free at order 2 cannot be mistaken for a free order-0 block
at the same address.

### The two bugs that shaped the free path

Both were memory corruption. Neither crashed.

**Extent.** `free` looked up the region containing an address but never checked that the whole
block *fit* inside it — while the coalescing loop directly below applied exactly that test to the
buddy. A block starting inside a region and ending past it went onto a free list, and the next
allocation of that order handed out memory the allocator never owned.

**Alignment.** After the extent fix, a review found the same hole one step over. `buddy = pa ^
size` is only the real buddy when `pa` is a multiple of `size`. A misaligned free was accepted,
and the next allocation returned a block overlapping a live one. The fuzzer could not have found
it: the harness rounded addresses down to a block-size multiple, so misaligned input was
unreachable by construction.

The general lesson is recorded in the repo: **a fuzz target that normalises its inputs cannot find
bugs in the normalisation.**

## Kernel heap

The heap is a segregated-fit allocator with 17 size classes at 3/2 spacing rather than pure powers
of two. Doubling caps worst-case internal waste at 49% — a 1025-byte request consuming 2048;
interleaving the 1.5× steps caps it at 33%. The cost is eight more list heads (64 bytes) and one
extra comparison in class selection, which stays branch-light and division-free.

Anything larger than the biggest class is rounded to a power-of-two extent and tracked in parallel
large-block lists. Without that, the bump region never recycles and a 16 MiB non-growing heap is
exhausted by roughly twice the peak size of every large object ever allocated.

### Why `alloc` is a safe function and `dealloc` is not

`SlabHeap::alloc` was originally `unsafe` with the comment "standard `GlobalAlloc::alloc`
contract". That named an obligation the caller could neither discover nor discharge. There is
nothing for the caller to uphold at that call site: the invariants are established by
`set_backing` and preserved by `dealloc`, both of which remain `unsafe`.

Returning a raw pointer is not unsafe; *dereferencing* it is, and that is the caller's act.
Marking `alloc` unsafe devalued the marker on the call sitting right next to it that genuinely
carries a contract.

## Per-CPU state and SMP

### Why `static mut` had to go

M0 kept the GDT, TSS, IDT and double-fault stack in `static mut` singletons. That was sound only
because exactly one CPU existed. The TSS holds `rsp0` and the interrupt stack table, both per-CPU
by definition — two cores sharing one means two cores faulting onto the same stack.

They now live in a per-CPU block reached through `GS`. The first five fields sit at fixed offsets
with compile-time offset assertions, because the `SYSCALL` entry stub reaches them with `gs:[N]`
before any Rust runs. A field reorder would otherwise be discovered by a userspace process reading
another process's stack, so it is a compile error instead.

### Why the bootstrap processor's block is static and the others are heap-allocated

The obvious design allocates every per-CPU block. That cannot work for the bootstrap processor:
descriptor tables must exist before the frame allocator and heap are initialised, because a CPU
with no IDT triple-faults on the first fault instead of printing a diagnostic. So the BSP uses a
statically reserved block and only application processors allocate. Linux splits it the same way
for the same reason.

### Why application processors come online and then park {#smp}

Every AP installs its own descriptor tables and reports in. None of them run threads yet.

That is a specific limit, not an omission. The scheduler's `current` is a single field naming one
running thread. On one CPU that is true; with two CPUs scheduling through it, the first switch has
one core save its stack pointer into the other core's context — two threads on one stack, which is
the same class of failure as the allocator handing out one frame twice. The per-CPU block already
carries a `current_thread` slot; moving `current` into it is what makes APs schedulable.

Parking is the honest behaviour in the meantime: an AP that took work would corrupt the CPU that
queued it.

## Scheduling

Policy is a separate crate with no notion of a CPU, a stack, or a context switch. Threads are
opaque IDs, so the policy is host-tested where a wrong answer is a failed assertion rather than a
machine that stops responding.

Three priority bands with FIFO inside each. Three rather than a numeric nice value because that is
enough to express "the idle thread must never preempt real work" and "the tick must not be
starved"; a weighted scheme with no workload to tune against would be guesswork.

Two properties earned their place by being non-obvious:

- **Enqueueing a thread that is already queued is refused.** A double push lets one thread be
  handed to two CPUs — the scheduler's version of the double-free.
- **"Is there work to steal" excludes the idle band.** The idle thread is always queued, so a
  naive length check would migrate idle threads between cores forever.

### The two ordering rules that make preemption safe

**The scheduler lock is never held across a context switch.** Holding it would deadlock the moment
the incoming thread scheduled: the lock would be owned by a thread that is no longer running and
cannot release it until it is scheduled again.

**Interrupts stay off across the whole of `schedule`, not just while the lock is held.** The lock
must drop before the switch, and in that window `current` already names the incoming thread — a
tick landing there would save the outgoing thread's stack pointer into the incoming thread's
context.

That second rule created a third: a thread running for the *first* time never reaches the
restore, so it would run with interrupts masked forever. Every kernel thread now starts through a
shim that enables them, making a fresh thread indistinguishable from a resumed one.

### Why a thread cannot free its own stack

The thread's saved context lives *on* its stack, so freeing it while the thread is merely stopped
hands its registers to whoever allocates next. `exit` marks the thread and queues its ID; the next
thread to run reaps it. Reaping drops the stacks after releasing the scheduler lock, because
freeing takes the heap lock and holding both would order two locks in a way nothing else does.

## Address spaces

An address space owns its page tables and returns every frame on drop. The kernel half is shared
by *reference* — the top-level entries 256–512 are copied, not the tables beneath them — so a
later kernel mapping appears in every address space without walking them all.

The cost is that teardown must free only what it allocated, which is why the address space keeps
an explicit list rather than walking the table at drop. A walk cannot distinguish a frame this
address space allocated from one it merely mapped, and freeing a shared kernel table would unmap
the kernel out from under every other address space — including the one doing the freeing.

`activate` is the first code in the project to write CR3, which makes two things newly
load-bearing. The instruction after `mov cr3` is fetched through the *new* tables, so a root
without the kernel's own text mapped faults on the instruction that would have handled the fault.
And CR3's flags are preserved rather than zeroed, because they carry PCID bits on machines that
enable them.

Dropping an address space that CR3 still points at is refused outright: it would hand a live page
table to the next allocator caller, and the fault that followed would have no tables left to
report itself through.

## Userspace

### Why the native ABI borrows Linux's register convention

`rax` holds the syscall number; arguments arrive in `rdi`, `rsi`, `rdx`, `r10`, `r8`. The numbers
are qunix's own and deliberately not Linux's — a compatibility personality will translate those —
but the *register* convention matches so that layer needs no re-plumbing.

`r10` rather than `rcx` for the fourth argument because `SYSCALL` clobbers `rcx` with the return
address. That is architectural, not a choice.

### The GDT layout is dictated by SYSRET

User segments are appended user-*data* first, then user-*code*. `SYSRET` derives user SS from
`STAR[63:48] + 8` and user CS from `+ 16`, so the other order compiles, boots, and then returns to
ring 3 with a data selector in CS.

### Two kernel stack pointers, not one

`percpu.kernel_rsp` is read by the `SYSCALL` stub, which switches stacks itself because `syscall`
does not. `TSS.rsp0` is read by the *CPU* on any interrupt taken from ring 3. They are different
mechanisms and both must be set.

Setting only the first produces a kernel that services syscalls correctly and then dies on the
first timer tick that lands while a process is running, because the CPU pushes an interrupt frame
to address 0.

### W^X from the first program

A process's text is mapped executable and not writable; its stack is writable and never
executable. Text is mapped writable just long enough for the kernel to copy the program in, then
remapped — the alternative, a second temporary mapping of the same frame, costs a page table walk
and buys nothing, because nothing can reach the address space until it is activated.

### What syscall argument validation does and does not check

Addresses naming memory are checked: the higher half is refused outright, ranges that wrap are
refused, and a range that *starts* legal and *ends* kernel-side is refused — checking only the
start would let a process read across the boundary.

What is not checked is whether the pages are mapped. A well-formed but unmapped user address still
faults, and the kernel has no handler that can recover. That needs a per-thread expected-fault
hook, which is a fault-handling change rather than a syscall change. It is documented as the limit
rather than papered over.
