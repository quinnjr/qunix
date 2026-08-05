# M2 — Filesystems, Block I/O, and a Concurrent Kernel

Status: approved, not yet planned
Supersedes nothing. Depends on M1 (merged, PR #4).

## What M2 is

A kernel that boots its own root filesystem from a disk, reads and writes it
safely, and uses more than one CPU to do it.

Concretely: TLB shootdown, per-CPU run queues with work stealing, process
reaping, an async runtime, virtio-blk, a buffer cache, a VFS, tmpfs, devfs, and
ext4 with full read-write support including jbd2 journalling.

This is a larger milestone than M0 or M1, deliberately and with the scope
chosen explicitly rather than inherited. The alternative — splitting the
prerequisites and the filesystems into separate milestones — was considered and
rejected. What follows assumes the whole thing lands together, with the
prerequisites sequenced first inside it.

## Why the prerequisites are inside M2 rather than before it

Three of the limitations M1 carried forward are not independent work items.
They are preconditions for anything that touches a shared address space or
blocks on hardware:

- **No TLB shootdown.** M1's plan calls this "a live correctness bug rather
  than a theoretical one — it must be fixed before any address space is
  modified while shared." A buffer cache that maps pages into a process does
  exactly that. Built in the other order, every bug it causes presents as a
  filesystem bug.
- **APs online but idle, behind one global run queue.** Block I/O is the first
  thing in this kernel that blocks. With one run queue and one CPU able to run
  threads, the first `read()` that waits parks the machine rather than the
  thread.
- **No process reaping.** `PROCESSES` grows without bound. Harmless while
  nothing creates processes in a loop; not harmless once a shell does.

### The carried-limitations list has been reconciled

M1's "Known Limitations Carried Into M2" list was partly out of date when this
spec was written — three of its seven entries described code that had already
changed:

| M1's claim | Actual state |
| --- | --- |
| `sys_write` takes a value, not a buffer | False. It takes a real user buffer through `copy_user_slice`. |
| No user-pointer validation beyond a range check | False. The range check is joined by a per-page mapping check, so an unmapped user pointer returns `BadAddress` rather than faulting in ring 0. |
| `VmSpace::drop` reclaims only the PML4 frame | False. `owned: Vec<u64>` records every frame the address space allocated and drop returns all of them. |

Those entries have been **removed at the source**, in
`docs/superpowers/plans/2026-08-04-m1-processes.md`, rather than corrected only
here. A handoff list that is wrong in one document and right in another is
worse than one that is simply wrong, because the next reader has no way to know
which is current.

This is the same failure M1 hit and recorded as Execution Deviation D1: M0's
*planned* interfaces had drifted from its shipped ones, and five assumptions
had to be reconciled mid-milestone. Catching it during the spec costs an hour;
catching it during Task 6 costs a deviation.

In scope here: no TLB shootdown, APs idle, single global run queue, no process
reaping. Out of scope and still true: no FPU/SSE context switching, and a
process's address space is leaked on exit (Deviation D7).

## Design decisions

Each of these was a fork with real alternatives. Recording the reasoning
matters more than recording the choice, because the choice is visible in the
code and the reasoning is not.

### The VFS is an enum, not a trait object

```rust
enum Vnode {
    Tmpfs(TmpfsNode),
    Devfs(DevfsNode),
    Ext4(Ext4Node),
}
```

The starting point was a `dyn Vnode` trait, which is what both Linux
(`inode_operations`) and OpenBSD (`vnodeops`) use. It did not survive the
decision to make the kernel async: `async fn` in a trait is not dyn-compatible
without `Pin<Box<dyn Future>>`, which means a heap allocation on every read.

Allocating in the I/O path is a specific kernel hazard, not a general
performance concern: memory pressure triggers writeback, writeback needs I/O,
I/O needs the allocation that is already waiting. An enum makes every future a
concrete, compiler-sized state machine — no vtable, no boxing, no allocation.

The cost is a closed set. Adding a filesystem means adding a variant and
recompiling, rather than registering a driver at runtime. For a kernel with
three filesystems and no loadable modules, that is the right trade; it would be
the wrong trade for a kernel that wanted out-of-tree filesystems.

**This is a deliberate divergence from Linux, OpenBSD and Redox alike**, and
belongs in the comparisons write-up on the site.

### No dentry cache

Linux's dcache is a substantial subsystem — negative dentries, pruning,
revalidation — and historically the source of its nastiest races. M2 does path
resolution directly against the filesystem, with `Arc` refcounting where Linux
uses manual `dget`/`dput` and OpenBSD uses `vref`/`vrele`.

Caching is a measured optimisation, not a starting assumption. If path lookup
proves to be the bottleneck, the benchmark harness exists to show it.

### One path walker

Symlink-loop bounding, mountpoint crossing, `.` and `..` handling, and
permission checks happen in exactly one function. This is a direct response to
the defect class that dominated M1: every Critical found there was a correct
guard applied to one of two places. A single walker means there is no second
place.

### The kernel is async

`async fn` throughout the I/O paths, driven by a per-CPU executor.

The straightforward alternative was wait queues — a thread sleeps, an ISR wakes
it, kernel code stays straight-line synchronous. That is what Linux and the BSDs
do and it is meaningfully less work.

Async was chosen to answer a question the project was built to ask: **is an
asynchronous kernel possible at all, and does Rust supply the primitives that
make it testable?**

That is a positive hypothesis, not a search for a limit. Nobody ships a
mainstream general-purpose kernel built on `async`/`await` — Linux, the BSDs
and Redox all use blocking primitives with a thread per blocked operation. The
usual explanation is that the machinery is too costly to hand-build. In C it
would be: a state machine per suspension point, written out by hand, with the
compiler offering no help proving the resumption is correct and no way to name
the type of a partially-completed operation.

Rust makes the experiment tractable rather than merely tedious. `async fn`
generates the state machines, `Pin` encodes the address-stability requirement
those machines have in the type system rather than in a comment, and `Waker` is
a stable interface an interrupt handler can call without knowing what it wakes.
None of those are performance features; they are the reason the experiment can
be attempted by one person in a milestone instead of being a research project.

So M2 is a real test of that. If an async kernel is achievable, it should be
achievable here — the workload is right (block I/O is exactly the latency the
model exists for), the scale is small enough to hold in one head, and the
language supplies the parts. If it is not achievable, the specific reason it
fails is worth more than a working kernel built the ordinary way, and this
milestone should report it plainly rather than quietly retreating to wait
queues.

The known costs are accepted going in, not discovered: the boxing problem
above, waker construction with interrupts masked, and the fact that `async`
colours every signature it touches.

### Wakers never allocate and never take a contended lock

An ISR pushes a task id onto a lock-free per-CPU ready list; the scheduler
drains it. The ISR does not construct an `Arc`, does not allocate, and does not
touch a lock that a non-interrupt path could hold — the self-deadlock
`IrqSpinLock` exists to prevent.

### Per-CPU executors

Each CPU owns a run queue and an executor. A syscall that awaits parks its
thread and calls `schedule()`; the CPU picks up other work. A single global
executor would have been simpler and would have become the contention point
that moving the run queue per-CPU exists to remove.

## Layers

| Layer | Contents |
| --- | --- |
| Concurrency | IPI TLB shootdown with acknowledgement; `RunQueue` and `current` in `PerCpu`; work stealing; process reaping |
| Runtime | Per-CPU executor, park/unpark, lock-free ready lists, ISR-safe wakers |
| Block | virtio-blk over PCI, split virtqueues, MSI-X per queue |
| Cache | Page-granular buffer cache keyed by `(dev, block)`, dirty tracking |
| VFS | `enum Vnode`, path walker, mount table, per-process fd table, syscalls |
| Filesystems | tmpfs, devfs, ext4 |

## Task sequence

1. **T1 — TLB shootdown.** IPI-based, sender waits for acknowledgement.
2. **T2 — Per-CPU scheduling.** Run queue and `current` into `PerCpu`, work
   stealing via the existing `RunQueue::steal`, process reaping.
3. **T3 — Async runtime.** Per-CPU executor, park/unpark, ISR-safe wakers.
4. **T4 — virtio-blk.** PCI enumeration, virtqueues, MSI-X, completion futures.
5. **T5 — Buffer cache.** Page-granular, dirty tracking, writeback.
6. **T6 — VFS core.** `enum Vnode`, path walker, mount table, fd table,
   `open`/`read`/`write`/`close`/`lseek`/`readdir`/`stat`.
7. **T7 — tmpfs.**
8. **T8 — devfs.** `/dev/null`, `/dev/zero`, `/dev/console`.
9. **T9 — ext4 read.** Superblock, block groups, extent trees, htree
   directories, metadata checksum verification.
10. **T10 — jbd2 replay.** Replay a dirty journal on mount.
11. **T11 — jbd2 transactions and ext4 write.** Block and inode allocation,
    orphan handling, ordered writeback.
12. **T12 — Boot from disk.** Mount the root filesystem, load `init` from ext4
    rather than from a Limine module.

## Error handling

Everything read from disk is attacker-controlled input, in exactly the sense
the ELF loader's input is. A corrupt superblock, an extent pointing outside the
filesystem, an htree with a cycle, a directory entry whose record length
overruns its block, a journal descriptor claiming more blocks than the journal
holds — these are **expected inputs**, refused with an errno.

No panics on filesystem content. The rule that produced `qunix-elf`, where
twelve of sixteen tests assert refusal, applies unchanged.

## Testing

The negative direction is where this milestone is won or lost.

**Fuzzing is the headline.** ext4's on-disk structures are the strongest fuzz
target this project has had — stronger than the allocators, because the input
is genuinely hostile rather than merely arbitrary. Three targets, each with an
independent model rather than a crash check:

- extent-tree walking: no extent may resolve outside the filesystem, and no two
  logical blocks may map to the same physical block within one inode
- htree lookup: termination, and every entry reachable by lookup is also
  reachable by a linear scan
- journal replay: replaying a prefix of a journal twice produces the same
  filesystem as replaying it once

The harness lessons already paid for apply: a target that normalises its inputs
cannot find bugs in the normalisation, and a model that is wrong looks exactly
like a bug.

**Shootdown is tested for what it prevents.** Asserting that an IPI was sent
tests nothing. The failure is a CPU that still resolves a stale translation
after the sender returns, so that is what the test constructs.

**Work stealing gets forced contention.** Per CLAUDE.md, a path that only runs
under contention is covered as a property of the host's core count unless the
test creates the contention deliberately. Steal paths must fail when not
exercised rather than merely go uncovered.

**jbd2 replay is tested against deliberately interrupted journals**, which
means generating those images as part of the harness rather than hoping a crash
produces one.

## Risks

- **Allocation in the writeback path.** Designed against from T5, not
  discovered at T11. The buffer cache must be able to write back without
  calling the general allocator.
- **Wakers from interrupt context.** The sharpest unknown, and the point where
  the async hypothesis is most likely to fail. If it does, the specific reason
  is the milestone's most valuable output and must be written up rather than
  worked around silently.
- **Scope.** Twelve tasks, several larger than M1's. The prerequisites are
  sequenced first specifically so that a mid-milestone stop still leaves the
  kernel better than it started.

## Out of scope

- FPU/SSE context switching (carried from M1)
- Address-space reclamation on process exit (Deviation D7)
- Loadable filesystem modules — precluded by the enum dispatch decision, and
  deliberately
- Block device partitioning, LVM, RAID
- Any filesystem other than tmpfs, devfs, ext4
