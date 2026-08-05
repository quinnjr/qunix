# qunix M1 — Processes and Userspace Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Prerequisite:** M0 complete — see `docs/superpowers/plans/2026-08-04-m0-boot-and-core.md`. This plan is written against M0's *planned* interfaces. Before starting Task 1, re-read the M0 source as it actually landed and correct any signature drift in this document; do not assume the two agree.

**Goal:** Run a real userspace process. Bring up per-CPU state and SMP, add preemptive threading with a scheduler, create isolated address spaces, define the native qunix syscall ABI, load a static ELF64 binary, and enter ring 3.

**Architecture:** Scheduling policy and ELF parsing are pure logic in host-tested crates (`qunix-sched`, `qunix-elf`); the kernel supplies the arch-specific context switch, per-CPU storage, and `SYSCALL`/`SYSRET` plumbing. A process is an address space plus a file-descriptor table plus a thread group; a thread is the scheduling unit. The Linux personality layer is explicitly **not** part of this milestone — only the native qunix ABI exists.

**Tech Stack:** Rust (pinned nightly, edition 2024), `limine` 0.6.5 MP request for SMP bring-up, `x86_64` 0.15, naked functions via `#[unsafe(naked)]` + `naked_asm!`.

## Execution Deviations

Recorded as they are found, per CLAUDE.md. The plan was written against M0's
*planned* interfaces; this section is the reconciliation against what M0
actually landed, plus anything reality contradicted during execution.

### D1 — Reconciliation before Task 1 (2026-08-05)

Re-read of the M0 source as it shipped. Five assumptions in this plan are wrong:

1. **`gdt::{KERNEL_CODE, USER_DATA}` do not exist as constants.** M0 landed
   accessor *functions* — `kernel_code_selector()`, `kernel_data_selector()`,
   `tss_selector()` — because the selectors are computed when the table is
   built rather than being fixed indices. There are **no user segments at all**;
   Task 8 must add them, not merely reference them. Tasks 1 and 8 are corrected
   to call the accessors.

2. **`AddressSpace::new_empty(hhdm_offset, root_pa)` does not exist.** M0 landed
   `unsafe fn AddressSpace::from_root(hhdm_offset, root_pa)`, which is the same
   thing under another name and is `unsafe` because it trusts `root_pa`. Task 7
   uses `from_root`; `new_empty` is not added.

3. **`AddressSpace::activate` does not exist.** Nothing in M0 ever reloads CR3 —
   the kernel runs on the bootloader's tables. Task 7 must add it, and it is the
   first code in the project to write CR3, so the TLB and the "are we still
   mapped afterwards" question are live for the first time.

4. **`qunix-abi` already exists.** Task 8 Step 1 says "create the ABI crate"; M0
   created it for `ExitCode` and the `HOST_STATUS_*` values that `xtask` matches
   on. Task 8 **extends** it. Nothing in it may be renamed without updating
   `xtask/src/qemu.rs`, which is a host-side consumer.

5. **`AddressSpace::translate` takes `&mut self`,** not `&self`, because the
   `x86_64` crate's `OffsetPageTable` needs a mutable mapper. Any caller the
   plan shows holding a shared borrow needs adjusting.

Unchanged and confirmed: `frames::alloc(order) -> Option<u64>`,
`unsafe frames::free(pa, order)`, `boot::hhdm_offset()`, `apic::eoi()`,
`apic::TIMER_VECTOR`, `qunix_mm::PAGE_SIZE`, and the `SpinLock`/`IrqSpinLock`
API. The `static mut` GDT/IDT the plan promises to remove are indeed still
there, in `gdt.rs` and `idt.rs`.

### D2 — The BSP cannot heap-allocate its per-CPU block (Task 1, 2026-08-05)

The plan's `unsafe fn percpu::install(cpu_id)` `Box`es the block. That is
impossible for the bootstrap processor. `gdt::init` runs at `kmain` line 101,
*before* `frames::init` and `heap::init`, and it has to: a CPU with no IDT
triple-faults on the first fault instead of printing a diagnostic, so the
descriptor tables must exist before the allocators run, not after.

Split into two entry points instead:

- `unsafe fn percpu::install_bsp()` — uses a statically reserved block, so it
  needs no allocator. Idempotent, because the in-QEMU harness brings the CPU up
  once per test and `ltr` refuses an already-busy TSS descriptor, so the tables
  must be rebuilt each time; `installed_count` is not incremented twice.
- `unsafe fn percpu::install_ap(cpu_id)` — boxes and leaks, for Task 6's APs,
  which start long after the heap is up.

Linux splits it the same way and for the same reason.

Also changed from the plan: `PerCpu` carries a `self_ptr` at offset 0x20.
`gs:`-relative addressing can read *through* the base but cannot produce it, so
recovering `&PerCpu` needs a pointer stored inside the block. The plan's
`current()` had no way to work as written.

### D3 — APs come online but do not schedule (Task 6, 2026-08-05)

Task 6 brings every application processor up: each installs its own per-CPU
block — GDT, TSS, IDT, double-fault stack — and reports in. It then parks in
`hlt`.

APs deliberately do **not** run scheduler threads yet, and the reason is
specific rather than a matter of effort. `sched::Scheduler::current` is a
single field naming one running thread. On one CPU that is the truth; with two
CPUs scheduling it is one "what am I running" slot shared between them, and the
first switch would have one CPU save its stack pointer into the other CPU's
context — two threads on one stack, which is the failure this project has
already shipped twice in the allocator.

`percpu::PerCpu` already carries a `current_thread` slot for exactly this.
Moving `current` (and then the run queue) into it is what makes APs
schedulable. Until then, parking is the honest behaviour: an AP that took work
would corrupt the CPU that queued it.

Also changed: `xtask` now launches QEMU with `-smp 4`. On a single-CPU guest
every AP assertion is vacuously true — it would assert that zero processors
came online, which is equally true of a kernel that cannot start any.

## Global Constraints

- **MSRV:** `rust-version = "1.97"` in every crate manifest.
- **Edition:** `2024`. `#[unsafe(no_mangle)]`, `#[unsafe(link_section)]`, `#[unsafe(naked)]` — the bare forms do not compile.
- **Target:** `targets/x86_64-qunix-kernel.json`, `-Z build-std=core,compiler_builtins,alloc`.
- **No floating point in kernel code.** The target disables SSE.
- **Licence:** all crates in this milestone are `MIT OR Apache-2.0`.
- **`static mut` is banned from this milestone onward.** M0 used it for the GDT and IDT under a single-CPU assumption; Task 1 removes it. New code uses per-CPU storage or `SpinLock`.
- **Every commit must leave `cargo xtask test` passing.**

## New Crates

Two crates are added beyond the spec's §3 table. Both are pure logic and exist so they can be host-tested:

| Crate | Responsibility |
| --- | --- |
| `qunix-sched` | Run queues and scheduling policy; no arch or hardware dependency |
| `qunix-elf` | ELF64 parsing and segment enumeration; no allocation, no I/O |

## File Structure

| Path | Responsibility |
| --- | --- |
| `crates/qunix-hal-x86_64/src/percpu.rs` | Per-CPU block reached through `GS` |
| `crates/qunix-hal-x86_64/src/context.rs` | Kernel-thread context switch |
| `crates/qunix-hal-x86_64/src/syscall.rs` | `SYSCALL`/`SYSRET` MSR setup and entry stub |
| `crates/qunix-hal-x86_64/src/gdt.rs` | Extended: user segments, per-CPU TSS |
| `crates/qunix-sched/src/lib.rs` | `RunQueue`, `Priority`, `SchedDecision` |
| `crates/qunix-elf/src/lib.rs` | `Elf64`, `ProgramHeader`, `SegmentFlags` |
| `crates/qunix-abi/src/lib.rs` | Native syscall numbers, argument structs, error codes |
| `kernel/src/thread.rs` | `Thread`, kernel stacks, thread lifecycle |
| `kernel/src/process.rs` | `Process`, address space ownership, PID allocation |
| `kernel/src/vmspace.rs` | Address-space creation and teardown |
| `kernel/src/sched.rs` | Kernel-side scheduler: per-CPU run queues, `schedule()`, `yield_now()` |
| `kernel/src/smp.rs` | Application-processor bring-up |
| `kernel/src/syscall.rs` | Native syscall dispatch |
| `kernel/src/loader.rs` | Maps an ELF into a fresh address space |

---

### Task 1: Per-CPU State

Removes M0's `static mut` GDT/IDT and gives every CPU its own descriptor tables, TSS, and scratch area, reached through `GS` and the `IA32_KERNEL_GS_BASE` MSR.

**Files:**
- Create: `crates/qunix-hal-x86_64/src/percpu.rs`
- Modify: `crates/qunix-hal-x86_64/src/gdt.rs`, `src/idt.rs`, `src/lib.rs`, `kernel/src/main.rs`

**Interfaces:**
- Consumes: `qunix_mm::PAGE_SIZE`, kernel heap (`alloc`).
- Produces:
  - `percpu::PerCpu { cpu_id: u32, kernel_rsp: u64, user_rsp: u64, current_thread: *mut (), gdt: GlobalDescriptorTable, tss: TaskStateSegment, idt: InterruptDescriptorTable }`
  - `unsafe fn percpu::install(cpu_id: u32)` — allocates and installs this CPU's block
  - `fn percpu::current() -> &'static PerCpu`
  - `unsafe fn percpu::current_mut() -> &'static mut PerCpu`
  - `fn percpu::cpu_id() -> u32`

- [ ] **Step 1: Write the failing test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn percpu_block_is_reachable_and_reports_its_id() {
        use qunix_hal_x86_64::percpu;
        crate::frames::init();
        crate::heap::init();
        unsafe { percpu::install(0) };
        assert_eq!(percpu::cpu_id(), 0);
        assert_eq!(percpu::current().cpu_id, 0);

        // Writing through the per-CPU block must be visible on the next read.
        unsafe { percpu::current_mut().kernel_rsp = 0xffff_ffff_dead_0000 };
        assert_eq!(percpu::current().kernel_rsp, 0xffff_ffff_dead_0000);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find percpu in qunix_hal_x86_64`.

- [ ] **Step 3: Implement the per-CPU block**

```rust
// crates/qunix-hal-x86_64/src/percpu.rs
extern crate alloc;

use alloc::boxed::Box;
use core::arch::asm;
use x86_64::structures::gdt::GlobalDescriptorTable;
use x86_64::structures::idt::InterruptDescriptorTable;
use x86_64::structures::tss::TaskStateSegment;

const IA32_GS_BASE: u32 = 0xC000_0101;
const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// Per-CPU state. The first two fields are at fixed offsets because the
/// syscall entry stub reaches them with `gs:[offset]` before any Rust runs.
#[repr(C)]
pub struct PerCpu {
    /// offset 0x00 — stack the syscall stub switches to
    pub kernel_rsp: u64,
    /// offset 0x08 — scratch slot for the user stack pointer
    pub user_rsp: u64,
    /// offset 0x10
    pub cpu_id: u32,
    _pad: u32,
    /// offset 0x18 — opaque pointer to the running `Thread`
    pub current_thread: *mut (),
    pub gdt: GlobalDescriptorTable,
    pub tss: TaskStateSegment,
    pub idt: InterruptDescriptorTable,
}

pub const OFFSET_KERNEL_RSP: usize = 0x00;
pub const OFFSET_USER_RSP: usize = 0x08;

fn write_msr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high,
             options(nomem, nostack, preserves_flags));
    }
}

/// Allocates this CPU's block and points `GS` at it.
///
/// # Safety
/// Must be called exactly once per CPU, after the kernel heap is online and
/// before any code touches `percpu::current`.
pub unsafe fn install(cpu_id: u32) {
    let block = Box::new(PerCpu {
        kernel_rsp: 0,
        user_rsp: 0,
        cpu_id,
        _pad: 0,
        current_thread: core::ptr::null_mut(),
        gdt: GlobalDescriptorTable::new(),
        tss: TaskStateSegment::new(),
        idt: InterruptDescriptorTable::new(),
    });
    let ptr = Box::into_raw(block) as u64;
    // Both MSRs are set so that `swapgs` on syscall entry is correct no matter
    // which of the two is currently live.
    write_msr(IA32_GS_BASE, ptr);
    write_msr(IA32_KERNEL_GS_BASE, ptr);
}

/// `gs:[offset]` can read *through* the base but cannot yield the base itself,
/// so the block's address comes back from the MSR.
fn base() -> *mut PerCpu {
    read_gs_base() as *mut PerCpu
}

fn read_gs_base() -> u64 {
    let (high, low): (u32, u32);
    unsafe {
        asm!("rdmsr", in("ecx") IA32_GS_BASE, out("eax") low, out("edx") high,
             options(nomem, nostack, preserves_flags));
    }
    ((high as u64) << 32) | low as u64
}

pub fn current() -> &'static PerCpu {
    unsafe { &*base() }
}

/// # Safety
/// The caller must ensure no other reference to this CPU's block is live.
/// Interrupts should be disabled for the duration of the borrow.
pub unsafe fn current_mut() -> &'static mut PerCpu {
    unsafe { &mut *base() }
}

pub fn cpu_id() -> u32 {
    current().cpu_id
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo xtask test`
Expected: PASS.

- [ ] **Step 5: Move the GDT and IDT into the per-CPU block**

```rust
// crates/qunix-hal-x86_64/src/gdt.rs  (replace the whole file)
use crate::percpu;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, SegmentSelector};

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
const IST_STACK_PAGES: usize = 5;

/// Selector values are fixed by the layout `SYSCALL`/`SYSRET` require:
/// kernel code, kernel data, user data, user code — in that order.
pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
pub const USER_DATA: u16 = 0x18;
pub const USER_CODE: u16 = 0x20;

/// Builds and loads this CPU's GDT and TSS.
///
/// # Safety
/// `percpu::install` must have run on this CPU, and the kernel heap must be online.
pub unsafe fn init() {
    extern crate alloc;
    use alloc::vec;

    let cpu = unsafe { percpu::current_mut() };

    let ist_stack = vec![0u8; IST_STACK_PAGES * 4096].into_boxed_slice();
    let ist_top = VirtAddr::from_ptr(ist_stack.as_ptr()) + ist_stack.len() as u64;
    core::mem::forget(ist_stack); // the IST stack lives for the life of the CPU
    cpu.tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = ist_top;

    let kcode = cpu.gdt.append(Descriptor::kernel_code_segment());
    let kdata = cpu.gdt.append(Descriptor::kernel_data_segment());
    let udata = cpu.gdt.append(Descriptor::user_data_segment());
    let ucode = cpu.gdt.append(Descriptor::user_code_segment());
    // SAFETY: the TSS lives inside the per-CPU block, which is never freed.
    let tss_sel = cpu.gdt.append(Descriptor::tss_segment(unsafe {
        &*(&raw const cpu.tss)
    }));

    assert_eq!(kcode.0, KERNEL_CODE, "kernel code selector moved");
    assert_eq!(kdata.0, KERNEL_DATA, "kernel data selector moved");
    assert_eq!(udata.0 & !3, USER_DATA, "user data selector moved");
    assert_eq!(ucode.0 & !3, USER_CODE, "user code selector moved");

    unsafe {
        cpu.gdt.load_unsafe();
        CS::set_reg(kcode);
        DS::set_reg(kdata);
        ES::set_reg(kdata);
        SS::set_reg(kdata);
        load_tss(tss_sel);
    }
}

pub fn kernel_code_selector() -> SegmentSelector {
    SegmentSelector(KERNEL_CODE)
}

pub fn user_code_selector() -> SegmentSelector {
    SegmentSelector(USER_CODE | 3)
}

pub fn user_data_selector() -> SegmentSelector {
    SegmentSelector(USER_DATA | 3)
}
```

The selector order is asserted rather than assumed. `SYSRET` derives the user code and stack selectors arithmetically from `IA32_STAR`, so if this order ever changes silently, ring-3 entry breaks in a way that is very hard to diagnose. Task 8 depends on these exact values.

Apply the equivalent change to `idt.rs`: replace `static mut IDT` with `percpu::current_mut().idt`, and make both `init` and `set_handler` `unsafe fn`.

- [ ] **Step 6: Update the boot path and the M0 GDT test**

```rust
// kernel/src/main.rs  (in kmain, replacing the M0 gdt/idt calls)
    frames::init();
    heap::init();
    unsafe { qunix_hal_x86_64::percpu::install(0) };
    unsafe { qunix_hal_x86_64::gdt::init() };
    unsafe { qunix_hal_x86_64::idt::init() };
```

The heap must come before `percpu::install`, which allocates. Update the M0 tests that called `gdt::init()`/`idt::init()` directly to call `percpu::install(0)` first and wrap the calls in `unsafe`.

- [ ] **Step 7: Run the full suite**

Run: `cargo xtask test`
Expected: PASS, including the M0 breakpoint and double-fault tests.

- [ ] **Step 8: Commit**

```bash
git add crates/qunix-hal-x86_64 kernel
git commit -m "refactor(hal): per-cpu gdt, tss, and idt; remove static mut"
```

---

### Task 2: Scheduling Policy (`qunix-sched`, Host-Tested)

Pure data structure and policy, with no notion of a CPU, a stack, or a context switch. Threads are represented by an opaque `ThreadId`.

**Files:**
- Create: `crates/qunix-sched/Cargo.toml`, `crates/qunix-sched/src/lib.rs`
- Test: `crates/qunix-sched/src/lib.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: `alloc`.
- Produces:
  - `ThreadId(pub u64)`
  - `Priority { Idle, Normal, High }`
  - `RunQueue::new() -> RunQueue`
  - `RunQueue::push(&mut self, id: ThreadId, prio: Priority)`
  - `RunQueue::pop(&mut self) -> Option<ThreadId>`
  - `RunQueue::len(&self) -> usize`, `is_empty(&self) -> bool`
  - `RunQueue::steal(&mut self) -> Option<ThreadId>` — takes from the *back* of the lowest non-empty band, for work stealing
  - `RunQueue::remove(&mut self, id: ThreadId) -> bool`

- [ ] **Step 1: Create the crate manifest**

```toml
# crates/qunix-sched/Cargo.toml
[package]
name = "qunix-sched"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[features]
default = []
std = []
```

- [ ] **Step 2: Write the failing tests**

```rust
// crates/qunix-sched/src/lib.rs  (append)
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_queue_is_empty() {
        let q = RunQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn pop_returns_none_when_empty() {
        let mut q = RunQueue::new();
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn single_priority_is_first_in_first_out() {
        let mut q = RunQueue::new();
        for i in 1..=3 {
            q.push(ThreadId(i), Priority::Normal);
        }
        assert_eq!(q.pop(), Some(ThreadId(1)));
        assert_eq!(q.pop(), Some(ThreadId(2)));
        assert_eq!(q.pop(), Some(ThreadId(3)));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn higher_priority_runs_before_lower() {
        let mut q = RunQueue::new();
        q.push(ThreadId(1), Priority::Idle);
        q.push(ThreadId(2), Priority::Normal);
        q.push(ThreadId(3), Priority::High);
        assert_eq!(q.pop(), Some(ThreadId(3)));
        assert_eq!(q.pop(), Some(ThreadId(2)));
        assert_eq!(q.pop(), Some(ThreadId(1)));
    }

    #[test]
    fn fifo_order_is_preserved_within_a_priority_band() {
        let mut q = RunQueue::new();
        q.push(ThreadId(1), Priority::High);
        q.push(ThreadId(2), Priority::Normal);
        q.push(ThreadId(3), Priority::High);
        assert_eq!(q.pop(), Some(ThreadId(1)));
        assert_eq!(q.pop(), Some(ThreadId(3)));
        assert_eq!(q.pop(), Some(ThreadId(2)));
    }

    #[test]
    fn len_tracks_pushes_and_pops() {
        let mut q = RunQueue::new();
        q.push(ThreadId(1), Priority::Normal);
        q.push(ThreadId(2), Priority::High);
        assert_eq!(q.len(), 2);
        q.pop();
        assert_eq!(q.len(), 1);
        q.pop();
        assert!(q.is_empty());
    }

    #[test]
    fn steal_takes_the_least_urgent_work_from_the_back() {
        let mut q = RunQueue::new();
        q.push(ThreadId(1), Priority::Normal);
        q.push(ThreadId(2), Priority::Normal);
        q.push(ThreadId(3), Priority::High);
        // The victim keeps what it would run next; the thief takes the tail of
        // the least urgent band.
        assert_eq!(q.steal(), Some(ThreadId(2)));
        assert_eq!(q.pop(), Some(ThreadId(3)));
        assert_eq!(q.pop(), Some(ThreadId(1)));
    }

    #[test]
    fn steal_returns_none_when_only_one_thread_remains() {
        let mut q = RunQueue::new();
        q.push(ThreadId(1), Priority::Normal);
        assert_eq!(q.steal(), None, "stealing the last thread starves the victim");
    }

    #[test]
    fn remove_finds_a_thread_in_any_band() {
        let mut q = RunQueue::new();
        q.push(ThreadId(1), Priority::Idle);
        q.push(ThreadId(2), Priority::High);
        assert!(q.remove(ThreadId(1)));
        assert!(!q.remove(ThreadId(1)));
        assert_eq!(q.len(), 1);
        assert_eq!(q.pop(), Some(ThreadId(2)));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p qunix-sched --features std --target x86_64-unknown-linux-musl`
Expected: FAIL — `cannot find type RunQueue`.

- [ ] **Step 4: Implement the run queue**

```rust
// crates/qunix-sched/src/lib.rs  (top of file)
#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::collections::VecDeque;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct ThreadId(pub u64);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Priority {
    Idle = 0,
    Normal = 1,
    High = 2,
}

const BANDS: usize = 3;

/// A fixed-band priority run queue. Each band is FIFO, and higher bands are
/// always drained first. Simple and starvation-prone by construction; ageing
/// belongs in a later milestone once there is a workload to tune against.
pub struct RunQueue {
    bands: [VecDeque<ThreadId>; BANDS],
    len: usize,
}

impl RunQueue {
    pub fn new() -> Self {
        Self { bands: [const { VecDeque::new() }; BANDS], len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, id: ThreadId, prio: Priority) {
        self.bands[prio as usize].push_back(id);
        self.len += 1;
    }

    pub fn pop(&mut self) -> Option<ThreadId> {
        for band in self.bands.iter_mut().rev() {
            if let Some(id) = band.pop_front() {
                self.len -= 1;
                return Some(id);
            }
        }
        None
    }

    /// Takes work for another CPU: the tail of the *lowest* non-empty band, so
    /// the victim keeps the thread it was about to run.
    pub fn steal(&mut self) -> Option<ThreadId> {
        if self.len < 2 {
            return None;
        }
        for band in self.bands.iter_mut() {
            if let Some(id) = band.pop_back() {
                self.len -= 1;
                return Some(id);
            }
        }
        None
    }

    pub fn remove(&mut self, id: ThreadId) -> bool {
        for band in self.bands.iter_mut() {
            if let Some(pos) = band.iter().position(|&t| t == id) {
                band.remove(pos);
                self.len -= 1;
                return true;
            }
        }
        false
    }
}

impl Default for RunQueue {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p qunix-sched --features std --target x86_64-unknown-linux-musl`
Expected: PASS, 9 tests.

- [ ] **Step 6: Add the crate to xtask's host-test list**

```rust
// xtask/src/main.rs  (in the "test" arm)
            for package in ["qunix-sync", "qunix-mm", "qunix-sched", "qunix-elf"] {
```

`qunix-elf` is created in Task 9; adding it now means only one edit to this list. It will fail until then, so make this edit in Task 9 instead if working strictly task-by-task.

- [ ] **Step 7: Commit**

```bash
git add crates/qunix-sched xtask
git commit -m "feat(sched): priority run queue with work stealing"
```

---

### Task 3: Kernel Thread Context Switch

**Files:**
- Create: `crates/qunix-hal-x86_64/src/context.rs`
- Modify: `crates/qunix-hal-x86_64/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `context::Context` — `#[repr(C)]` holding callee-saved registers and `rip`
  - `unsafe fn context::switch(from: *mut *mut Context, to: *mut Context)`
  - `unsafe fn context::init_kernel_stack(stack_top: u64, entry: extern "C" fn(u64) -> !, arg: u64) -> *mut Context`

- [ ] **Step 1: Write the failing test**

`switch` writes the outgoing thread's context pointer into `*from` as part of
the switch, so a shared rendezvous slot is all the child needs to get back.
A `SpinLock` holds that slot: `static mut` is banned in M1, and a bare
`*mut Context` is not `Send`.

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn context_switch_runs_a_second_kernel_thread_and_returns() {
        extern crate alloc;
        use alloc::vec;
        use core::sync::atomic::{AtomicU64, Ordering};
        use qunix_hal_x86_64::context::{self, Context};
        use qunix_sync::SpinLock;

        crate::frames::init();
        crate::heap::init();

        static REACHED: AtomicU64 = AtomicU64::new(0);
        /// Where the main thread's saved context lands, so the child can return to it.
        static MAIN_CTX: SpinLock<ContextPtr> = SpinLock::new(ContextPtr(core::ptr::null_mut()));

        extern "C" fn thread_entry(arg: u64) -> ! {
            REACHED.store(arg, Ordering::SeqCst);
            let back = MAIN_CTX.lock().0;
            let mut discard = ContextPtr(core::ptr::null_mut());
            unsafe { context::switch(&raw mut discard.0, back) };
            unreachable!("resumed a thread that already finished");
        }

        let stack = vec![0u8; 64 * 1024].into_boxed_slice();
        let top = stack.as_ptr() as u64 + stack.len() as u64;
        let child = unsafe { context::init_kernel_stack(top, thread_entry, 0xabcd) };

        unsafe { context::switch(&raw mut MAIN_CTX.lock().0, child) };

        assert_eq!(REACHED.load(Ordering::SeqCst), 0xabcd);
        drop(stack);
    }
```

Add the pointer wrapper at kernel module scope, since a bare `*mut Context` is not `Send`:

```rust
// kernel/src/main.rs  (at module scope)
#[derive(Clone, Copy)]
pub struct ContextPtr(pub *mut qunix_hal_x86_64::context::Context);
unsafe impl Send for ContextPtr {}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find context in qunix_hal_x86_64`.

- [ ] **Step 3: Implement the context switch**

```rust
// crates/qunix-hal-x86_64/src/context.rs
use core::arch::naked_asm;

/// Callee-saved state for a kernel thread, laid out exactly as `switch` pushes it.
#[repr(C)]
pub struct Context {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbx: u64,
    pub rbp: u64,
    /// Address `ret` will jump to when this context is resumed.
    pub rip: u64,
}

/// Saves the current callee-saved registers, stores the resulting context
/// pointer in `*from`, then resumes `to`.
///
/// # Safety
/// `from` must be a valid, writable location. `to` must point at a context
/// produced either by a previous `switch` or by `init_kernel_stack`.
#[unsafe(naked)]
pub unsafe extern "C" fn switch(from: *mut *mut Context, to: *mut Context) {
    naked_asm!(
        // Push in reverse field order so RSP ends up pointing at a `Context`.
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",   // *from = current context
        "mov rsp, rsi",     // switch to the target stack
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",              // jumps to Context::rip
    );
}

/// Trampoline that moves the saved argument into the C argument register
/// before calling the thread entry point.
#[unsafe(naked)]
unsafe extern "C" fn thread_trampoline() -> ! {
    naked_asm!(
        // `init_kernel_stack` left [rsp] = arg and [rsp+8] = entry.
        "pop rdi",
        "pop rax",
        "call rax",
        // A thread entry point returns `!`, so reaching here is a bug.
        "ud2",
    );
}

/// Prepares a fresh kernel stack so that switching to the returned context
/// begins executing `entry(arg)`.
///
/// # Safety
/// `stack_top` must be the exclusive, 16-byte-alignable top of a writable
/// region of at least a few pages that outlives the thread.
pub unsafe fn init_kernel_stack(
    stack_top: u64,
    entry: extern "C" fn(u64) -> !,
    arg: u64,
) -> *mut Context {
    let mut sp = stack_top & !0xF;

    // The trampoline pops `arg` then `entry`, so push them in reverse.
    sp -= 8;
    unsafe { (sp as *mut u64).write(entry as usize as u64) };
    sp -= 8;
    unsafe { (sp as *mut u64).write(arg) };

    sp -= core::mem::size_of::<Context>() as u64;
    let ctx = sp as *mut Context;
    unsafe {
        ctx.write(Context {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbx: 0,
            rbp: 0,
            rip: thread_trampoline as usize as u64,
        });
    }
    ctx
}
```

Register the module:
```rust
// crates/qunix-hal-x86_64/src/lib.rs
pub mod context;
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo xtask test`
Expected: PASS.

If the machine triple-faults instead, the usual cause is stack misalignment: the System V ABI requires `RSP % 16 == 8` at the point a `call` target begins executing. Verify by adding `println!` calls around the switch and checking how far it gets before dying.

- [ ] **Step 5: Commit**

```bash
git add crates/qunix-hal-x86_64 kernel
git commit -m "feat(hal): kernel thread context switch"
```

---

### Task 4: Threads and the Kernel Scheduler

**Files:**
- Create: `kernel/src/thread.rs`, `kernel/src/sched.rs`
- Modify: `kernel/src/main.rs`, `kernel/Cargo.toml`

**Interfaces:**
- Consumes: `qunix_sched::{RunQueue, Priority, ThreadId}`, `context::{Context, switch, init_kernel_stack}`, `percpu`.
- Produces:
  - `thread::Thread { id: ThreadId, context: *mut Context, stack: Box<[u8]>, state: ThreadState, priority: Priority, process: Option<Arc<Process>> }`
  - `thread::ThreadState { Ready, Running, Blocked, Exited }`
  - `sched::init()`
  - `sched::spawn_kernel(entry: extern "C" fn(u64) -> !, arg: u64, prio: Priority) -> ThreadId`
  - `sched::yield_now()`
  - `sched::current_id() -> ThreadId`
  - `sched::exit_current() -> !`
  - `sched::thread_count() -> usize`

- [ ] **Step 1: Write the failing test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn scheduler_round_robins_between_kernel_threads() {
        use core::sync::atomic::{AtomicU64, Ordering};
        use qunix_sched::Priority;

        crate::boot_prelude();
        crate::sched::init();

        static COUNTER: AtomicU64 = AtomicU64::new(0);

        extern "C" fn worker(id: u64) -> ! {
            for _ in 0..10 {
                COUNTER.fetch_add(1 << (id * 8), Ordering::SeqCst);
                crate::sched::yield_now();
            }
            crate::sched::exit_current();
        }

        crate::sched::spawn_kernel(worker, 0, Priority::Normal);
        crate::sched::spawn_kernel(worker, 1, Priority::Normal);

        // Yield until both workers have exited.
        let mut budget = 10_000;
        while crate::sched::thread_count() > 1 && budget > 0 {
            crate::sched::yield_now();
            budget -= 1;
        }

        assert!(budget > 0, "workers never finished");
        let counter = COUNTER.load(Ordering::SeqCst);
        assert_eq!(counter & 0xff, 10, "worker 0 did not run ten times");
        assert_eq!((counter >> 8) & 0xff, 10, "worker 1 did not run ten times");
    }
```

Add a shared setup helper so every test from here on has the same prelude:

```rust
// kernel/src/main.rs  (at module scope)
/// Brings the kernel to the state `kmain` reaches before scheduling starts.
/// Idempotent, so tests may call it freely.
pub fn boot_prelude() {
    frames::init();
    heap::init();
    unsafe { qunix_hal_x86_64::percpu::install(0) };
    unsafe { qunix_hal_x86_64::gdt::init() };
    unsafe { qunix_hal_x86_64::idt::init() };
}
```

`percpu::install` is not idempotent as written in Task 1 — it allocates a new block each call. Make it idempotent by returning early when `read_gs_base() != 0`, and update its doc comment accordingly.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find sched in the crate root`.

- [ ] **Step 3: Implement threads**

```rust
// kernel/src/thread.rs
extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use qunix_hal_x86_64::context::Context;
use qunix_sched::{Priority, ThreadId};

pub const KERNEL_STACK_SIZE: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadState {
    Ready,
    Running,
    Blocked,
    Exited,
}

pub struct Thread {
    pub id: ThreadId,
    /// Saved context; valid only while the thread is not running.
    pub context: *mut Context,
    pub stack: Box<[u8]>,
    pub state: ThreadState,
    pub priority: Priority,
    pub process: Option<Arc<crate::process::Process>>,
    /// Top of the kernel stack, loaded into the TSS when this thread runs.
    pub kernel_stack_top: u64,
}

// A `Thread` is only ever reachable through the scheduler's lock.
unsafe impl Send for Thread {}

impl Thread {
    pub fn new_kernel(
        id: ThreadId,
        entry: extern "C" fn(u64) -> !,
        arg: u64,
        priority: Priority,
    ) -> Self {
        let stack = alloc::vec![0u8; KERNEL_STACK_SIZE].into_boxed_slice();
        let top = stack.as_ptr() as u64 + stack.len() as u64;
        let context = unsafe { qunix_hal_x86_64::context::init_kernel_stack(top, entry, arg) };
        Self {
            id,
            context,
            stack,
            state: ThreadState::Ready,
            priority,
            process: None,
            kernel_stack_top: top,
        }
    }
}
```

- [ ] **Step 4: Implement the scheduler**

```rust
// kernel/src/sched.rs
extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};
use qunix_hal_x86_64::context::{self, Context};
use qunix_sched::{Priority, RunQueue, ThreadId};
use qunix_sync::SpinLock;

use crate::thread::{Thread, ThreadState};

struct Scheduler {
    ready: RunQueue,
    threads: BTreeMap<ThreadId, Box<Thread>>,
    current: Option<ThreadId>,
    initialised: bool,
}

static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler {
    ready: RunQueue::new_const(),
    threads: BTreeMap::new(),
    current: None,
    initialised: false,
});

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Registers the currently executing code as thread 1 so that `yield_now` has
/// something to switch away from. Idempotent.
pub fn init() {
    let mut s = SCHED.lock();
    if s.initialised {
        return;
    }
    let id = ThreadId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
    // The bootstrap thread already has a stack — the one Limine gave us — so it
    // gets an empty placeholder and a null context that `switch` fills in.
    let mut boot = Thread::new_kernel(id, bootstrap_never_runs, 0, Priority::Normal);
    boot.context = core::ptr::null_mut();
    boot.state = ThreadState::Running;
    s.threads.insert(id, Box::new(boot));
    s.current = Some(id);
    s.initialised = true;
}

extern "C" fn bootstrap_never_runs(_: u64) -> ! {
    panic!("the bootstrap thread's entry point was invoked");
}

pub fn current_id() -> ThreadId {
    SCHED.lock().current.expect("sched::init not called")
}

pub fn thread_count() -> usize {
    SCHED.lock().threads.len()
}

pub fn spawn_kernel(entry: extern "C" fn(u64) -> !, arg: u64, prio: Priority) -> ThreadId {
    let id = ThreadId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let thread = Thread::new_kernel(id, entry, arg, prio);
    let mut s = SCHED.lock();
    s.threads.insert(id, Box::new(thread));
    s.ready.push(id, prio);
    id
}

/// Picks the next ready thread and switches to it. Returns when this thread is
/// scheduled again.
pub fn yield_now() {
    // Interrupts stay off across the pick so the timer cannot re-enter us
    // while the scheduler lock is held.
    let were_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();

    let switch_pair = {
        let mut s = SCHED.lock();
        let Some(next_id) = s.ready.pop() else {
            drop(s);
            if were_enabled {
                x86_64::instructions::interrupts::enable();
            }
            return;
        };
        let prev_id = s.current.expect("sched::init not called");
        if next_id == prev_id {
            None
        } else {
            let prev_prio = s.threads[&prev_id].priority;
            if s.threads[&prev_id].state == ThreadState::Running {
                s.threads.get_mut(&prev_id).unwrap().state = ThreadState::Ready;
                s.ready.push(prev_id, prev_prio);
            }
            let next = s.threads.get_mut(&next_id).unwrap();
            next.state = ThreadState::Running;
            let next_ctx = next.context;
            let next_stack_top = next.kernel_stack_top;
            s.current = Some(next_id);
            let prev_ctx_slot: *mut *mut Context =
                &raw mut s.threads.get_mut(&prev_id).unwrap().context;
            Some((prev_ctx_slot, next_ctx, next_stack_top))
        }
    };

    if let Some((prev_slot, next_ctx, next_stack_top)) = switch_pair {
        // The TSS must point at the incoming thread's kernel stack before it
        // can take an interrupt or a syscall from ring 3.
        unsafe {
            let cpu = qunix_hal_x86_64::percpu::current_mut();
            cpu.tss.privilege_stack_table[0] = x86_64::VirtAddr::new(next_stack_top);
            cpu.kernel_rsp = next_stack_top;
        }
        unsafe { context::switch(prev_slot, next_ctx) };
    }

    if were_enabled {
        x86_64::instructions::interrupts::enable();
    }
}

/// Marks the running thread exited and switches away permanently.
pub fn exit_current() -> ! {
    x86_64::instructions::interrupts::disable();
    loop {
        let switch_to = {
            let mut s = SCHED.lock();
            let id = s.current.expect("sched::init not called");
            s.threads.get_mut(&id).unwrap().state = ThreadState::Exited;
            match s.ready.pop() {
                Some(next_id) => {
                    s.threads.remove(&id);
                    let next = s.threads.get_mut(&next_id).unwrap();
                    next.state = ThreadState::Running;
                    let ctx = next.context;
                    let top = next.kernel_stack_top;
                    s.current = Some(next_id);
                    Some((ctx, top))
                }
                None => None,
            }
        };

        match switch_to {
            Some((ctx, top)) => unsafe {
                let cpu = qunix_hal_x86_64::percpu::current_mut();
                cpu.tss.privilege_stack_table[0] = x86_64::VirtAddr::new(top);
                cpu.kernel_rsp = top;
                let mut discard: *mut Context = core::ptr::null_mut();
                context::switch(&raw mut discard, ctx);
                unreachable!("resumed an exited thread");
            },
            None => {
                // Nothing else to run. Idle with interrupts on so the timer can
                // wake us when another CPU makes work available.
                x86_64::instructions::interrupts::enable();
                x86_64::instructions::hlt();
                x86_64::instructions::interrupts::disable();
            }
        }
    }
}
```

`RunQueue::new()` is not `const`, but the scheduler needs a `const` initialiser for its `static`. Add a `const fn new_const()` to `qunix-sched` alongside `new()`:

```rust
// crates/qunix-sched/src/lib.rs  (in impl RunQueue)
    pub const fn new_const() -> Self {
        Self { bands: [const { VecDeque::new() }; BANDS], len: 0 }
    }
```

`VecDeque::new()` is `const`, so `new()` can simply call `new_const()`. Replace the body of `new()` accordingly and add a test asserting `RunQueue::new_const()` is usable in a `static`.

- [ ] **Step 5: Register the modules and run the test**

```rust
// kernel/src/main.rs
mod sched;
mod thread;
```

Add `qunix-sched.workspace = true` to `kernel/Cargo.toml`.

Run: `cargo xtask test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/qunix-sched kernel
git commit -m "feat(sched): kernel threads with cooperative scheduling"
```

---

### Task 5: Timer-Driven Preemption

**Files:**
- Modify: `kernel/src/main.rs`, `kernel/src/sched.rs`

**Interfaces:**
- Consumes: `apic::eoi`, `sched::yield_now`.
- Produces: `sched::preempt()` — the timer-safe entry point; `sched::set_preemption(bool)`.

- [ ] **Step 1: Write the failing test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn timer_preempts_a_thread_that_never_yields() {
        use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use qunix_sched::Priority;

        crate::boot_prelude();
        crate::sched::init();
        crate::install_timer();
        qunix_hal_x86_64::apic::init(crate::boot::hhdm_offset());
        qunix_hal_x86_64::apic::start_timer(0b1011, 1_000_000);
        crate::sched::set_preemption(true);

        static SPUN: AtomicU64 = AtomicU64::new(0);
        static STOP: AtomicBool = AtomicBool::new(false);

        extern "C" fn spinner(_: u64) -> ! {
            // Deliberately never calls yield_now.
            while !STOP.load(Ordering::Relaxed) {
                SPUN.fetch_add(1, Ordering::Relaxed);
                core::hint::spin_loop();
            }
            crate::sched::exit_current();
        }

        crate::sched::spawn_kernel(spinner, 0, Priority::Normal);
        x86_64::instructions::interrupts::enable();

        // If preemption works, the spinner runs without us ever yielding to it.
        let mut budget = 500_000_000u64;
        while SPUN.load(Ordering::Relaxed) == 0 && budget > 0 {
            core::hint::spin_loop();
            budget -= 1;
        }
        STOP.store(true, Ordering::Relaxed);

        // Let it exit.
        let mut drain = 10_000;
        while crate::sched::thread_count() > 1 && drain > 0 {
            crate::sched::yield_now();
            drain -= 1;
        }
        x86_64::instructions::interrupts::disable();
        crate::sched::set_preemption(false);

        assert!(budget > 0, "the spinner was never scheduled — preemption failed");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `cannot find function set_preemption`.

- [ ] **Step 3: Implement preemption**

```rust
// kernel/src/sched.rs  (append)
use core::sync::atomic::AtomicBool;

static PREEMPT_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_preemption(enabled: bool) {
    PREEMPT_ENABLED.store(enabled, Ordering::Release);
}

/// Called from the timer interrupt after EOI. Safe to call when the scheduler
/// is not initialised or preemption is off — it simply does nothing.
pub fn preempt() {
    if !PREEMPT_ENABLED.load(Ordering::Acquire) {
        return;
    }
    // If the scheduler lock is already held, this interrupt landed inside the
    // scheduler itself. Skipping the switch is correct: the holder will yield.
    if SCHED.try_lock().is_none() {
        return;
    }
    yield_now();
}
```

`SpinLock::try_lock` returns a guard, which must be dropped before `yield_now` takes the lock again. Bind and drop it explicitly rather than relying on temporary lifetimes:

```rust
    match SCHED.try_lock() {
        Some(guard) => drop(guard),
        None => return,
    }
    yield_now();
```

```rust
// kernel/src/main.rs  (in timer_handler, after eoi)
extern "x86-interrupt" fn timer_handler(_frame: InterruptStackFrame) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    qunix_hal_x86_64::apic::eoi();
    sched::preempt();
}
```

Calling `eoi()` before the switch matters: the APIC must be released before this CPU disappears into another thread, or no further timer interrupts arrive.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo xtask test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add kernel
git commit -m "feat(sched): timer-driven preemption"
```

---

### Task 6: SMP Bring-Up

**Files:**
- Create: `kernel/src/smp.rs`
- Modify: `kernel/src/boot.rs`, `kernel/src/main.rs`

**Interfaces:**
- Consumes: Limine MP request, `percpu::install`, `gdt::init`, `idt::init`.
- Produces:
  - `smp::start_all()` — brings up every application processor
  - `smp::cpu_count() -> u32`
  - `smp::online_count() -> u32`

- [ ] **Step 1: Write the failing test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn all_application_processors_come_online() {
        crate::boot_prelude();
        crate::sched::init();

        let expected = crate::smp::cpu_count();
        assert!(expected >= 2, "qemu must be launched with -smp 4 for this test");

        crate::smp::start_all();

        let mut budget = 200_000_000u64;
        while crate::smp::online_count() < expected && budget > 0 {
            core::hint::spin_loop();
            budget -= 1;
        }
        assert_eq!(
            crate::smp::online_count(),
            expected,
            "only {} of {expected} CPUs came online",
            crate::smp::online_count()
        );
    }
```

- [ ] **Step 2: Give QEMU more than one CPU**

```rust
// xtask/src/qemu.rs  (in run_iso, with the other args)
    cmd.args(["-smp", "4"]);
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find smp in the crate root`.

- [ ] **Step 4: Add the Limine MP request**

```rust
// kernel/src/boot.rs  (append to the request block)
use limine::request::MpRespData;

#[used]
#[unsafe(link_section = ".requests")]
pub static MP: Request<MpRespData> = Request::new();
```

- [ ] **Step 5: Verify the MP response API against the generated docs**

Run: `cargo doc -p limine --no-deps --target x86_64-unknown-linux-musl`
Open `target/doc/limine/mp/index.html` and `target/doc/limine/request/struct.MpRespData.html`.

Confirm the accessor used below — iterating CPUs and writing each one's `goto_address` to start it — matches 0.6.5's names. Correct the code from the docs before continuing rather than guessing.

- [ ] **Step 6: Implement SMP bring-up**

```rust
// kernel/src/smp.rs
use core::sync::atomic::{AtomicU32, Ordering};

static ONLINE: AtomicU32 = AtomicU32::new(1); // the bootstrap processor

pub fn cpu_count() -> u32 {
    crate::boot::MP
        .response()
        .map(|r| r.cpus().len() as u32)
        .unwrap_or(1)
}

pub fn online_count() -> u32 {
    ONLINE.load(Ordering::Acquire)
}

/// Entry point every application processor lands on. Limine passes a pointer to
/// the CPU's own descriptor in RDI.
extern "C" fn ap_entry(cpu: &limine::mp::Cpu) -> ! {
    unsafe {
        qunix_hal_x86_64::percpu::install(cpu.id);
        qunix_hal_x86_64::gdt::init();
        qunix_hal_x86_64::idt::init();
    }
    qunix_hal_x86_64::apic::init(crate::boot::hhdm_offset());
    ONLINE.fetch_add(1, Ordering::AcqRel);

    // M1 runs one run queue on the bootstrap processor; APs idle until M1's
    // follow-up work gives them their own queues. Idling with interrupts on
    // keeps them responsive to IPIs.
    x86_64::instructions::interrupts::enable();
    loop {
        x86_64::instructions::hlt();
    }
}

/// Starts every application processor. Idempotent.
pub fn start_all() {
    let Some(response) = crate::boot::MP.response() else {
        return;
    };
    let bsp_id = response.bsp_lapic_id();
    for cpu in response.cpus() {
        if cpu.lapic_id == bsp_id {
            continue;
        }
        // Writing goto_address is what actually releases the AP.
        cpu.goto_address.write(ap_entry);
    }
}
```

`percpu::install` allocates from the shared kernel heap, and several APs may reach it simultaneously. The heap is already behind a `SpinLock`, so this is safe — but confirm the lock is genuinely held across the whole allocation before trusting it, because a heap that is only *mostly* thread-safe fails intermittently and is miserable to debug.

- [ ] **Step 7: Run the test to verify it passes**

Run: `cargo xtask test`
Expected: PASS, reporting 4 CPUs online.

- [ ] **Step 8: Start APs at boot**

```rust
// kernel/src/main.rs  (in kmain, after sched::init)
    smp::start_all();
    println!("qunix: {} cpus online", smp::online_count());
```

- [ ] **Step 9: Commit**

```bash
git add kernel xtask
git commit -m "feat(smp): bring up application processors via limine mp"
```

---

### Task 7: Address Spaces

**Files:**
- Create: `kernel/src/vmspace.rs`
- Modify: `crates/qunix-hal-x86_64/src/paging.rs`, `kernel/src/main.rs`

**Interfaces:**
- Consumes: `frames::alloc`, `AddressSpace`, `boot::hhdm_offset`.
- Produces:
  - `AddressSpace::new_empty(hhdm_offset: u64, root_pa: u64) -> AddressSpace` — wraps a fresh PML4
  - `AddressSpace::root_frame(&self) -> u64`
  - `unsafe fn AddressSpace::activate(&self)` — loads CR3
  - `vmspace::VmSpace` — owns a PML4 frame and frees it on drop
  - `vmspace::VmSpace::new() -> VmSpace` — copies the kernel's higher-half entries
  - `vmspace::VmSpace::map_user(&mut self, va: u64, pa: u64, writable: bool, executable: bool)`
  - `unsafe fn vmspace::VmSpace::activate(&self)`

- [ ] **Step 1: Write the failing test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn a_new_address_space_shares_the_kernel_half_but_not_the_user_half() {
        crate::boot_prelude();
        let mut space = crate::vmspace::VmSpace::new();

        const USER_VA: u64 = 0x0000_4000_0000;
        let pa = crate::frames::alloc(0).unwrap();
        space.map_user(USER_VA, pa, true, false);

        // The kernel half must still be reachable from the new space, or
        // activating it would fault instantly on the next instruction fetch.
        let kernel_va = crate::kmain as usize as u64;
        assert!(space.translate(kernel_va).is_some(), "kernel text is not mapped");

        // The user mapping must not be visible in the currently active space.
        let current = unsafe {
            qunix_hal_x86_64::paging::AddressSpace::active(crate::boot::hhdm_offset())
        };
        assert!(current.translate(USER_VA).is_none(), "user mapping leaked into the kernel space");

        assert_eq!(space.translate(USER_VA), Some(pa));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find vmspace in the crate root`.

- [ ] **Step 3: Extend the paging layer**

```rust
// crates/qunix-hal-x86_64/src/paging.rs  (append to impl AddressSpace)
    /// Wraps a caller-supplied, already-zeroed PML4 frame.
    ///
    /// # Safety
    /// `root_pa` must be an exclusively owned, zeroed 4 KiB frame, and the
    /// whole of physical memory must be mapped at `hhdm_offset`.
    pub unsafe fn from_root(hhdm_offset: u64, root_pa: u64) -> Self {
        let virt = VirtAddr::new(hhdm_offset + root_pa);
        let table: &'static mut PageTable = unsafe { &mut *virt.as_mut_ptr() };
        let mapper = unsafe { OffsetPageTable::new(table, VirtAddr::new(hhdm_offset)) };
        Self { mapper }
    }

    pub fn root_frame(&self) -> u64 {
        // level_4_table() borrows the same table `from_root` wrapped.
        let table = self.mapper.level_4_table();
        let virt = VirtAddr::from_ptr(table as *const PageTable);
        virt.as_u64() - self.mapper.phys_offset().as_u64()
    }

    /// Copies the kernel's higher-half PML4 entries (indices 256..512) into
    /// this table, so kernel addresses stay valid after `activate`.
    pub fn share_kernel_half(&mut self, source: &AddressSpace) {
        let src = source.mapper.level_4_table();
        let dst = self.mapper.level_4_table();
        for index in 256..512 {
            dst[index] = src[index].clone();
        }
    }

    /// # Safety
    /// Every address the current code, stack, and data live at must remain
    /// mapped in this space, or the very next instruction faults.
    pub unsafe fn activate(&self) {
        use x86_64::registers::control::{Cr3, Cr3Flags};
        let frame = PhysFrame::containing_address(PhysAddr::new(self.root_frame()));
        unsafe { Cr3::write(frame, Cr3Flags::empty()) };
    }
```

`OffsetPageTable::phys_offset()` exists in `x86_64` 0.15; if the accessor is named differently, store the offset in `AddressSpace` as a field instead of reading it back. Prefer storing it — it removes the dependency on that accessor entirely:

```rust
pub struct AddressSpace {
    mapper: OffsetPageTable<'static>,
    hhdm_offset: u64,
}
```

Update `active`, `from_root`, and `root_frame` to set and use the field. Do this in the same edit; a half-converted struct will not compile.

- [ ] **Step 4: Implement `VmSpace`**

```rust
// kernel/src/vmspace.rs
use crate::{boot, frames};
use qunix_hal_x86_64::paging::{AddressSpace, PageFlags};

/// An owned user address space. Frees its root frame on drop.
pub struct VmSpace {
    space: AddressSpace,
    root_pa: u64,
}

impl VmSpace {
    pub fn new() -> Self {
        let hhdm = boot::hhdm_offset();
        let root_pa = frames::alloc(0).expect("out of frames allocating a PML4");
        // A fresh PML4 must be fully zeroed or it will contain garbage entries.
        unsafe { core::ptr::write_bytes((hhdm + root_pa) as *mut u8, 0, 4096) };

        let mut space = unsafe { AddressSpace::from_root(hhdm, root_pa) };
        let kernel = unsafe { AddressSpace::active(hhdm) };
        space.share_kernel_half(&kernel);

        Self { space, root_pa }
    }

    pub fn map_user(&mut self, va: u64, pa: u64, writable: bool, executable: bool) {
        let mut flags = PageFlags::PRESENT | PageFlags::USER;
        if writable {
            flags = flags | PageFlags::WRITABLE;
        }
        if !executable {
            flags = flags | PageFlags::NO_EXECUTE;
        }
        unsafe {
            self.space
                .map(va, pa, flags, &mut || frames::alloc(0))
                .expect("failed to map a user page");
        }
    }

    pub fn translate(&self, va: u64) -> Option<u64> {
        self.space.translate(va)
    }

    pub fn root_frame(&self) -> u64 {
        self.root_pa
    }

    /// # Safety
    /// The caller must be executing from kernel addresses, which this space
    /// shares, and must not hold references into the outgoing user mapping.
    pub unsafe fn activate(&self) {
        unsafe { self.space.activate() };
    }
}

impl Drop for VmSpace {
    fn drop(&mut self) {
        // M1 leaks the intermediate page tables; only the root is reclaimed.
        // Full teardown lands with process reaping in M2.
        unsafe { frames::free(self.root_pa, 0) };
    }
}
```

Make `kmain` public so the test can take its address:
```rust
// kernel/src/main.rs
pub extern "C" fn kmain() -> ! {
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo xtask test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/qunix-hal-x86_64 kernel
git commit -m "feat(mm): user address spaces sharing the kernel half"
```

---

### Task 8: Native Syscall ABI and `SYSCALL`/`SYSRET`

**Files:**
- Create: `crates/qunix-abi/Cargo.toml`, `crates/qunix-abi/src/lib.rs`
- Create: `crates/qunix-hal-x86_64/src/syscall.rs`, `kernel/src/syscall.rs`
- Modify: `crates/qunix-hal-x86_64/src/lib.rs`, `kernel/src/main.rs`

**Interfaces:**
- Consumes: `gdt::{KERNEL_CODE, USER_DATA}`, `percpu::{OFFSET_KERNEL_RSP, OFFSET_USER_RSP}`.
- Produces:
  - `qunix_abi::Sys { Exit = 0, Write = 1, Yield = 2, GetPid = 3 }`
  - `qunix_abi::Errno { Ok = 0, BadSyscall = -1, BadAddress = -2, BadArgument = -3 }`
  - `unsafe fn hal::syscall::init(handler: SyscallHandler)` where
    `type SyscallHandler = extern "C" fn(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64`
  - `unsafe fn hal::syscall::enter_user(entry: u64, user_stack: u64) -> !`

**Register convention** (deliberately matching Linux, so the M3 personality layer needs no re-plumbing): `rax` = syscall number, arguments in `rdi`, `rsi`, `rdx`, `r10`, `r8`; return value in `rax`. `SYSCALL` clobbers `rcx` (saved RIP) and `r11` (saved RFLAGS).

- [ ] **Step 1: Create the ABI crate**

```toml
# crates/qunix-abi/Cargo.toml
[package]
name = "qunix-abi"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[features]
default = []
std = []
```

```rust
// crates/qunix-abi/src/lib.rs
#![cfg_attr(not(test), no_std)]

/// Native qunix syscall numbers. Deliberately disjoint from Linux numbering:
/// the M3 personality layer gets its own table rather than sharing this one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum Sys {
    Exit = 0,
    Write = 1,
    Yield = 2,
    GetPid = 3,
}

impl Sys {
    pub fn from_u64(value: u64) -> Option<Self> {
        match value {
            0 => Some(Self::Exit),
            1 => Some(Self::Write),
            2 => Some(Self::Yield),
            3 => Some(Self::GetPid),
            _ => None,
        }
    }
}

/// Negative return values from a syscall.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i64)]
pub enum Errno {
    BadSyscall = -1,
    BadAddress = -2,
    BadArgument = -3,
}

/// Highest address a userspace pointer may reach. The upper half is the kernel.
pub const USER_ADDRESS_LIMIT: u64 = 0x0000_8000_0000_0000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syscall_numbers_round_trip() {
        for sys in [Sys::Exit, Sys::Write, Sys::Yield, Sys::GetPid] {
            assert_eq!(Sys::from_u64(sys as u64), Some(sys));
        }
    }

    #[test]
    fn unknown_syscall_numbers_are_rejected() {
        assert_eq!(Sys::from_u64(4), None);
        assert_eq!(Sys::from_u64(u64::MAX), None);
    }

    #[test]
    fn user_limit_is_the_bottom_of_the_higher_half() {
        assert_eq!(USER_ADDRESS_LIMIT, 1 << 47);
    }
}
```

- [ ] **Step 2: Run the ABI tests**

Run: `cargo test -p qunix-abi --features std --target x86_64-unknown-linux-musl`
Expected: PASS, 3 tests. Add `qunix-abi` to xtask's host-test package list.

- [ ] **Step 3: Write the failing end-to-end test**

This test is the milestone's centrepiece: it enters ring 3 and comes back.

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn a_userspace_thread_can_issue_a_syscall_and_exit() {
        use core::sync::atomic::Ordering;

        crate::boot_prelude();
        crate::sched::init();
        crate::syscall::init();

        // A tiny ring-3 program: `mov rax, 1; mov rdi, 42; syscall; mov rax, 0; syscall`
        // i.e. Write(42) then Exit.
        const USER_CODE: &[u8] = &[
            0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00, // mov rax, 1  (Sys::Write)
            0x48, 0xC7, 0xC7, 0x2A, 0x00, 0x00, 0x00, // mov rdi, 42
            0x0F, 0x05,                               // syscall
            0x48, 0xC7, 0xC0, 0x00, 0x00, 0x00, 0x00, // mov rax, 0  (Sys::Exit)
            0x0F, 0x05,                               // syscall
        ];

        let pid = crate::process::spawn_raw(USER_CODE);

        let mut budget = 100_000;
        while !crate::syscall::test_probe_seen() && budget > 0 {
            crate::sched::yield_now();
            budget -= 1;
        }

        assert!(budget > 0, "the user thread never issued a syscall");
        assert_eq!(crate::syscall::test_probe_value(), 42);
        assert!(pid.0 > 0);
    }
```

`syscall::test_probe_seen` / `test_probe_value` are `#[cfg(test)]`-only hooks the `Write` handler sets, so the test observes ring-3 execution without needing a console protocol. Define them alongside the dispatcher in Step 6.

- [ ] **Step 4: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find syscall in the crate root`.

- [ ] **Step 5: Implement the `SYSCALL` entry path**

```rust
// crates/qunix-hal-x86_64/src/syscall.rs
use crate::gdt::{KERNEL_CODE, USER_DATA};
use crate::percpu::{OFFSET_KERNEL_RSP, OFFSET_USER_RSP};
use core::arch::{asm, naked_asm};
use core::sync::atomic::{AtomicU64, Ordering};

const IA32_EFER: u32 = 0xC000_0080;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;

const EFER_SCE: u64 = 1 << 0;

pub type SyscallHandler =
    extern "C" fn(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64;

static HANDLER: AtomicU64 = AtomicU64::new(0);

fn read_msr(msr: u32) -> u64 {
    let (high, low): (u32, u32);
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high,
             options(nomem, nostack, preserves_flags));
    }
    ((high as u64) << 32) | low as u64
}

fn write_msr(msr: u32, value: u64) {
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32,
             options(nomem, nostack, preserves_flags));
    }
}

/// Enables `SYSCALL`/`SYSRET` on this CPU and installs the dispatcher.
///
/// # Safety
/// The GDT must already have the layout `gdt.rs` asserts, and `percpu::install`
/// must have run on this CPU.
pub unsafe fn init(handler: SyscallHandler) {
    HANDLER.store(handler as usize as u64, Ordering::Release);

    // SYSCALL loads CS = STAR[47:32] and SS = that + 8.
    // SYSRET loads CS = STAR[63:48] + 16 and SS = that + 8.
    // With kernel base 0x08 and user base 0x10|3, that yields kernel 0x08/0x10
    // and user 0x23/0x1b — which is exactly the gdt.rs layout.
    let star = ((USER_DATA as u64 | 3) << 48) | ((KERNEL_CODE as u64) << 32);
    write_msr(IA32_STAR, star);
    write_msr(IA32_LSTAR, syscall_entry as usize as u64);
    // Mask IF and DF on entry: the handler runs with interrupts off until it
    // chooses otherwise, and string ops must not inherit a user DF.
    write_msr(IA32_FMASK, (1 << 9) | (1 << 10));
    write_msr(IA32_EFER, read_msr(IA32_EFER) | EFER_SCE);
}

#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    naked_asm!(
        "swapgs",                                   // GS now points at PerCpu
        "mov gs:[{user_rsp}], rsp",
        "mov rsp, gs:[{kernel_rsp}]",
        // Preserve what SYSRET needs and what the C ABI does not save.
        "push rcx",                                 // user RIP
        "push r11",                                 // user RFLAGS
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // Marshal into the C argument registers: handler(nr, a0..a4).
        "mov rcx, r10",                             // 4th C arg comes from r10
        "mov r9, r8",
        "mov r8, rcx",
        "mov rcx, rdx",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",
        "mov rax, [rip + {handler}]",
        "call rax",
        // Return value is already in RAX.
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "pop r11",
        "pop rcx",
        "mov rsp, gs:[{user_rsp}]",
        "swapgs",
        "sysretq",
        user_rsp = const OFFSET_USER_RSP,
        kernel_rsp = const OFFSET_KERNEL_RSP,
        handler = sym HANDLER,
    );
}

/// Drops to ring 3 at `entry` with `user_stack` as RSP. Never returns.
///
/// # Safety
/// `entry` and `user_stack` must be mapped user-accessible in the active
/// address space, and the TSS's RSP0 must already point at a valid kernel stack.
pub unsafe fn enter_user(entry: u64, user_stack: u64) -> ! {
    unsafe {
        asm!(
            "mov rsp, {stack}",
            "mov rcx, {entry}",     // SYSRET jumps to RCX
            "mov r11, {flags}",     // SYSRET loads RFLAGS from R11
            "swapgs",
            "sysretq",
            stack = in(reg) user_stack,
            entry = in(reg) entry,
            flags = in(reg) 0x202u64,   // IF set, reserved bit 1 set
            options(noreturn)
        );
    }
}
```

The `HANDLER` indirection is read through `rip`-relative addressing rather than being baked in, so the dispatcher can be installed after the stub is linked.

- [ ] **Step 6: Implement the dispatcher**

```rust
// kernel/src/syscall.rs
use qunix_abi::{Errno, Sys, USER_ADDRESS_LIMIT};

#[cfg(test)]
mod probe {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    pub static SEEN: AtomicBool = AtomicBool::new(false);
    pub static VALUE: AtomicU64 = AtomicU64::new(0);
    pub fn record(value: u64) {
        VALUE.store(value, Ordering::SeqCst);
        SEEN.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
pub fn test_probe_seen() -> bool {
    probe::SEEN.load(core::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
pub fn test_probe_value() -> u64 {
    probe::VALUE.load(core::sync::atomic::Ordering::SeqCst)
}

/// Installs the native syscall ABI on the current CPU.
pub fn init() {
    unsafe { qunix_hal_x86_64::syscall::init(dispatch) };
}

extern "C" fn dispatch(nr: u64, a0: u64, a1: u64, _a2: u64, _a3: u64, _a4: u64) -> i64 {
    match Sys::from_u64(nr) {
        Some(Sys::Exit) => crate::sched::exit_current(),
        Some(Sys::Write) => sys_write(a0, a1),
        Some(Sys::Yield) => {
            crate::sched::yield_now();
            0
        }
        Some(Sys::GetPid) => crate::process::current_pid().0 as i64,
        None => Errno::BadSyscall as i64,
    }
}

/// M1's `write` is deliberately minimal: it takes a value rather than a buffer,
/// because there are no file descriptors until M2's VFS exists.
fn sys_write(value: u64, _reserved: u64) -> i64 {
    if value >= USER_ADDRESS_LIMIT {
        return Errno::BadAddress as i64;
    }
    #[cfg(test)]
    probe::record(value);
    #[cfg(not(test))]
    qunix_hal_x86_64::println!("user write: {value}");
    0
}
```

- [ ] **Step 7: Run the test after Task 10 supplies `process::spawn_raw`**

`process::spawn_raw` and `process::current_pid` are defined in Task 10. Implement Tasks 9 and 10, then return here and run:

Run: `cargo xtask test`
Expected: PASS.

Because this test cannot pass until Task 10 lands, commit the syscall plumbing now and leave the test in place, expecting it to fail. That is the one point in this plan where a commit does not leave the suite green — note it in the commit message so it is not mistaken for a regression.

- [ ] **Step 8: Commit**

```bash
git add crates/qunix-abi crates/qunix-hal-x86_64 kernel xtask
git commit -m "feat(abi): native syscall numbers and SYSCALL/SYSRET entry path

The end-to-end ring-3 test is committed failing; it goes green once
process::spawn_raw lands in the following task."
```

---

### Task 9: ELF64 Loader (`qunix-elf`, Host-Tested)

**Files:**
- Create: `crates/qunix-elf/Cargo.toml`, `crates/qunix-elf/src/lib.rs`
- Test: `crates/qunix-elf/src/lib.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: nothing; parses a `&[u8]` with no allocation.
- Produces:
  - `Elf64::parse(bytes: &[u8]) -> Result<Elf64<'_>, ElfError>`
  - `Elf64::entry(&self) -> u64`
  - `Elf64::segments(&self) -> impl Iterator<Item = Segment<'_>>`
  - `Segment { vaddr: u64, mem_size: u64, data: &'a [u8], writable: bool, executable: bool }`
  - `ElfError { TooShort, BadMagic, NotX86_64, NotExecutable, BadProgramHeader }`

- [ ] **Step 1: Create the crate and write the failing tests**

```toml
# crates/qunix-elf/Cargo.toml
[package]
name = "qunix-elf"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[features]
default = []
std = []
```

```rust
// crates/qunix-elf/src/lib.rs  (append)
#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal, valid ELF64 executable with one PT_LOAD segment.
    fn minimal_elf(entry: u64, seg_vaddr: u64, payload: &[u8]) -> Vec<u8> {
        const EHDR: usize = 64;
        const PHDR: usize = 56;
        let data_off = EHDR + PHDR;
        let mut bytes = vec![0u8; data_off + payload.len()];

        bytes[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        bytes[4] = 2; // ELFCLASS64
        bytes[5] = 1; // ELFDATA2LSB
        bytes[6] = 1; // EV_CURRENT
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        bytes[18..20].copy_from_slice(&0x3Eu16.to_le_bytes()); // EM_X86_64
        bytes[24..32].copy_from_slice(&entry.to_le_bytes()); // e_entry
        bytes[32..40].copy_from_slice(&(EHDR as u64).to_le_bytes()); // e_phoff
        bytes[54..56].copy_from_slice(&(PHDR as u16).to_le_bytes()); // e_phentsize
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum

        let p = EHDR;
        bytes[p..p + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        bytes[p + 4..p + 8].copy_from_slice(&0b101u32.to_le_bytes()); // R+X
        bytes[p + 8..p + 16].copy_from_slice(&(data_off as u64).to_le_bytes()); // p_offset
        bytes[p + 16..p + 24].copy_from_slice(&seg_vaddr.to_le_bytes()); // p_vaddr
        bytes[p + 32..p + 40].copy_from_slice(&(payload.len() as u64).to_le_bytes()); // p_filesz
        bytes[p + 40..p + 48].copy_from_slice(&(payload.len() as u64 + 16).to_le_bytes()); // p_memsz

        bytes[data_off..].copy_from_slice(payload);
        bytes
    }

    #[test]
    fn parses_a_minimal_executable() {
        let bytes = minimal_elf(0x40_1000, 0x40_0000, &[0x90, 0x90]);
        let elf = Elf64::parse(&bytes).expect("parse failed");
        assert_eq!(elf.entry(), 0x40_1000);
    }

    #[test]
    fn enumerates_loadable_segments_with_their_data() {
        let payload = [0xAA, 0xBB, 0xCC];
        let bytes = minimal_elf(0x40_1000, 0x40_0000, &payload);
        let elf = Elf64::parse(&bytes).unwrap();
        let segments: Vec<_> = elf.segments().collect();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].vaddr, 0x40_0000);
        assert_eq!(segments[0].data, &payload);
        assert_eq!(segments[0].mem_size, payload.len() as u64 + 16);
        assert!(segments[0].executable);
        assert!(!segments[0].writable);
    }

    #[test]
    fn rejects_a_file_shorter_than_a_header() {
        assert_eq!(Elf64::parse(&[0x7f, b'E']), Err(ElfError::TooShort));
    }

    #[test]
    fn rejects_a_bad_magic_number() {
        let mut bytes = minimal_elf(0x1000, 0x1000, &[0x90]);
        bytes[1] = b'X';
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::BadMagic));
    }

    #[test]
    fn rejects_a_non_x86_64_machine() {
        let mut bytes = minimal_elf(0x1000, 0x1000, &[0x90]);
        bytes[18..20].copy_from_slice(&0xB7u16.to_le_bytes()); // EM_AARCH64
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::NotX86_64));
    }

    #[test]
    fn rejects_a_relocatable_object() {
        let mut bytes = minimal_elf(0x1000, 0x1000, &[0x90]);
        bytes[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::NotExecutable));
    }

    #[test]
    fn rejects_a_program_header_pointing_past_the_end_of_the_file() {
        let mut bytes = minimal_elf(0x1000, 0x1000, &[0x90]);
        // e_phoff far beyond the file.
        bytes[32..40].copy_from_slice(&0xFFFF_0000u64.to_le_bytes());
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::BadProgramHeader));
    }

    #[test]
    fn rejects_a_segment_whose_file_range_escapes_the_buffer() {
        let mut bytes = minimal_elf(0x1000, 0x1000, &[0x90]);
        let p = 64;
        bytes[p + 32..p + 40].copy_from_slice(&0xFFFF_u64.to_le_bytes()); // huge p_filesz
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::BadProgramHeader));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p qunix-elf --features std --target x86_64-unknown-linux-musl`
Expected: FAIL — `cannot find type Elf64`.

- [ ] **Step 3: Implement the parser**

```rust
// crates/qunix-elf/src/lib.rs  (top of file)
#![cfg_attr(not(test), no_std)]

const EI_NIDENT: usize = 16;
const EHDR_SIZE: usize = 64;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElfError {
    TooShort,
    BadMagic,
    NotX86_64,
    NotExecutable,
    BadProgramHeader,
}

#[derive(Clone, Copy, Debug)]
pub struct Segment<'a> {
    pub vaddr: u64,
    /// Bytes to reserve; may exceed `data.len()`, and the excess must be zeroed.
    pub mem_size: u64,
    pub data: &'a [u8],
    pub writable: bool,
    pub executable: bool,
}

pub struct Elf64<'a> {
    bytes: &'a [u8],
    entry: u64,
    phoff: usize,
    phentsize: usize,
    phnum: usize,
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

impl<'a> Elf64<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ElfError> {
        if bytes.len() < EHDR_SIZE {
            return Err(ElfError::TooShort);
        }
        if bytes[0..4] != [0x7f, b'E', b'L', b'F'] {
            return Err(ElfError::BadMagic);
        }
        if bytes[4] != 2 || bytes[5] != 1 {
            return Err(ElfError::NotX86_64); // not ELF64 little-endian
        }
        if u16_at(bytes, 18) != 0x3E {
            return Err(ElfError::NotX86_64);
        }
        if u16_at(bytes, 16) != 2 {
            return Err(ElfError::NotExecutable); // only ET_EXEC in M1
        }

        let entry = u64_at(bytes, 24);
        let phoff = u64_at(bytes, 32) as usize;
        let phentsize = u16_at(bytes, 54) as usize;
        let phnum = u16_at(bytes, 56) as usize;

        let table_end = phoff
            .checked_add(phentsize.checked_mul(phnum).ok_or(ElfError::BadProgramHeader)?)
            .ok_or(ElfError::BadProgramHeader)?;
        if phentsize < 56 || table_end > bytes.len() {
            return Err(ElfError::BadProgramHeader);
        }

        let elf = Self { bytes, entry, phoff, phentsize, phnum };
        // Validate every segment's file range up front, so `segments()` can be
        // infallible and callers cannot forget to check.
        for index in 0..phnum {
            let p = phoff + index * phentsize;
            if u32_at(bytes, p) != PT_LOAD {
                continue;
            }
            let offset = u64_at(bytes, p + 8) as usize;
            let filesz = u64_at(bytes, p + 32) as usize;
            let end = offset.checked_add(filesz).ok_or(ElfError::BadProgramHeader)?;
            if end > bytes.len() {
                return Err(ElfError::BadProgramHeader);
            }
            let memsz = u64_at(bytes, p + 40);
            if memsz < filesz as u64 {
                return Err(ElfError::BadProgramHeader);
            }
        }
        Ok(elf)
    }

    pub fn entry(&self) -> u64 {
        self.entry
    }

    pub fn segments(&self) -> impl Iterator<Item = Segment<'a>> + '_ {
        let bytes = self.bytes;
        let phoff = self.phoff;
        let phentsize = self.phentsize;
        (0..self.phnum).filter_map(move |index| {
            let p = phoff + index * phentsize;
            if u32_at(bytes, p) != PT_LOAD {
                return None;
            }
            let flags = u32_at(bytes, p + 4);
            let offset = u64_at(bytes, p + 8) as usize;
            let filesz = u64_at(bytes, p + 32) as usize;
            Some(Segment {
                vaddr: u64_at(bytes, p + 16),
                mem_size: u64_at(bytes, p + 40),
                data: &bytes[offset..offset + filesz],
                writable: flags & PF_W != 0,
                executable: flags & PF_X != 0,
            })
        })
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p qunix-elf --features std --target x86_64-unknown-linux-musl`
Expected: PASS, 8 tests.

- [ ] **Step 5: Add the crate to xtask's host-test list**

```rust
// xtask/src/main.rs
            for package in ["qunix-sync", "qunix-mm", "qunix-sched", "qunix-abi", "qunix-elf"] {
```

- [ ] **Step 6: Commit**

```bash
git add crates/qunix-elf xtask
git commit -m "feat(elf): validating elf64 parser with host tests"
```

---

### Task 10: Processes and Entering Ring 3

**Files:**
- Create: `kernel/src/process.rs`, `kernel/src/loader.rs`
- Modify: `kernel/src/thread.rs`, `kernel/src/sched.rs`, `kernel/src/main.rs`

**Interfaces:**
- Consumes: `VmSpace`, `qunix_elf::Elf64`, `hal::syscall::enter_user`, `sched::spawn_kernel`.
- Produces:
  - `process::Pid(pub u64)`
  - `process::Process { pid: Pid, space: SpinLock<VmSpace> }`
  - `process::spawn_raw(code: &[u8]) -> Pid` — maps raw machine code at a fixed address (used by tests and by the syscall test in Task 8)
  - `process::spawn_elf(image: &[u8]) -> Result<Pid, ElfError>`
  - `process::current_pid() -> Pid`
  - `loader::load(space: &mut VmSpace, image: &[u8]) -> Result<u64, ElfError>` — returns the entry point

- [ ] **Step 1: Write the failing test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn an_elf_image_loads_into_a_fresh_address_space() {
        crate::boot_prelude();

        // Same program as the ring-3 test, wrapped in a real ELF header.
        let image = crate::tests_support::minimal_user_elf();
        let mut space = crate::vmspace::VmSpace::new();
        let entry = crate::loader::load(&mut space, &image).expect("load failed");

        assert_eq!(entry, crate::tests_support::USER_ENTRY);
        // The entry page must be mapped, user-accessible, and executable.
        assert!(space.translate(entry).is_some(), "entry point is not mapped");
    }
```

Add the shared fixture:

```rust
// kernel/src/main.rs  (at module scope, gated to tests)
#[cfg(test)]
pub mod tests_support {
    extern crate alloc;
    use alloc::vec::Vec;

    pub const USER_ENTRY: u64 = 0x40_0000;

    /// `mov rax,1; mov rdi,42; syscall; mov rax,0; syscall`
    pub const USER_PROGRAM: &[u8] = &[
        0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00,
        0x48, 0xC7, 0xC7, 0x2A, 0x00, 0x00, 0x00,
        0x0F, 0x05,
        0x48, 0xC7, 0xC0, 0x00, 0x00, 0x00, 0x00,
        0x0F, 0x05,
    ];

    /// Builds a one-segment ET_EXEC image around `USER_PROGRAM`.
    pub fn minimal_user_elf() -> Vec<u8> {
        const EHDR: usize = 64;
        const PHDR: usize = 56;
        let data_off = EHDR + PHDR;
        let mut b = alloc::vec![0u8; data_off + USER_PROGRAM.len()];
        b[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        b[4] = 2;
        b[5] = 1;
        b[6] = 1;
        b[16..18].copy_from_slice(&2u16.to_le_bytes());
        b[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
        b[24..32].copy_from_slice(&USER_ENTRY.to_le_bytes());
        b[32..40].copy_from_slice(&(EHDR as u64).to_le_bytes());
        b[54..56].copy_from_slice(&(PHDR as u16).to_le_bytes());
        b[56..58].copy_from_slice(&1u16.to_le_bytes());

        let p = EHDR;
        b[p..p + 4].copy_from_slice(&1u32.to_le_bytes());
        b[p + 4..p + 8].copy_from_slice(&0b101u32.to_le_bytes());
        b[p + 8..p + 16].copy_from_slice(&(data_off as u64).to_le_bytes());
        b[p + 16..p + 24].copy_from_slice(&USER_ENTRY.to_le_bytes());
        b[p + 32..p + 40].copy_from_slice(&(USER_PROGRAM.len() as u64).to_le_bytes());
        b[p + 40..p + 48].copy_from_slice(&(USER_PROGRAM.len() as u64).to_le_bytes());
        b[data_off..].copy_from_slice(USER_PROGRAM);
        b
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo xtask test`
Expected: FAIL — `could not find loader in the crate root`.

- [ ] **Step 3: Implement the loader**

```rust
// kernel/src/loader.rs
use crate::frames;
use crate::vmspace::VmSpace;
use qunix_elf::{Elf64, ElfError};
use qunix_mm::PAGE_SIZE;

/// Maps every PT_LOAD segment of `image` into `space` and returns the entry point.
///
/// Copies through the HHDM rather than activating the target space, so the
/// caller keeps running in the kernel's address space throughout.
pub fn load(space: &mut VmSpace, image: &[u8]) -> Result<u64, ElfError> {
    let elf = Elf64::parse(image)?;
    let hhdm = crate::boot::hhdm_offset();

    for segment in elf.segments() {
        let start = segment.vaddr & !(PAGE_SIZE - 1);
        let end = (segment.vaddr + segment.mem_size).next_multiple_of(PAGE_SIZE);

        let mut va = start;
        while va < end {
            let pa = frames::alloc(0).expect("out of frames loading an ELF");
            // Zero first: BSS is the part of mem_size beyond the file data.
            unsafe { core::ptr::write_bytes((hhdm + pa) as *mut u8, 0, PAGE_SIZE as usize) };

            // Copy whatever part of this page the file actually covers.
            let page_start = va;
            let page_end = va + PAGE_SIZE;
            let data_start = segment.vaddr;
            let data_end = segment.vaddr + segment.data.len() as u64;
            let copy_start = page_start.max(data_start);
            let copy_end = page_end.min(data_end);
            if copy_start < copy_end {
                let src_offset = (copy_start - data_start) as usize;
                let dst_offset = (copy_start - page_start) as usize;
                let len = (copy_end - copy_start) as usize;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        segment.data.as_ptr().add(src_offset),
                        (hhdm + pa + dst_offset as u64) as *mut u8,
                        len,
                    );
                }
            }

            space.map_user(va, pa, segment.writable, segment.executable);
            va += PAGE_SIZE;
        }
    }

    Ok(elf.entry())
}
```

- [ ] **Step 4: Run the loader test to verify it passes**

Run: `cargo xtask test`
Expected: the loader test PASSES. The Task 8 ring-3 test still fails until Step 6.

- [ ] **Step 5: Implement processes**

```rust
// kernel/src/process.rs
extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use qunix_elf::ElfError;
use qunix_sched::Priority;
use qunix_sync::SpinLock;

use crate::vmspace::VmSpace;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Pid(pub u64);

pub struct Process {
    pub pid: Pid,
    pub space: SpinLock<VmSpace>,
    pub entry: u64,
    pub user_stack_top: u64,
}

static NEXT_PID: AtomicU64 = AtomicU64::new(1);
static PROCESSES: SpinLock<BTreeMap<Pid, Arc<Process>>> = SpinLock::new(BTreeMap::new());

const USER_STACK_TOP: u64 = 0x0000_7fff_0000_0000;
const USER_STACK_PAGES: u64 = 16;

fn create(entry: u64, mut space: VmSpace) -> Arc<Process> {
    // Map the user stack below a guard hole at USER_STACK_TOP.
    for page in 1..=USER_STACK_PAGES {
        let va = USER_STACK_TOP - page * qunix_mm::PAGE_SIZE;
        let pa = crate::frames::alloc(0).expect("out of frames mapping a user stack");
        unsafe {
            core::ptr::write_bytes(
                (crate::boot::hhdm_offset() + pa) as *mut u8,
                0,
                qunix_mm::PAGE_SIZE as usize,
            )
        };
        space.map_user(va, pa, true, false);
    }

    let pid = Pid(NEXT_PID.fetch_add(1, Ordering::Relaxed));
    let process = Arc::new(Process {
        pid,
        space: SpinLock::new(space),
        entry,
        user_stack_top: USER_STACK_TOP,
    });
    PROCESSES.lock().insert(pid, Arc::clone(&process));
    process
}

/// Spawns a process from raw machine code mapped at a fixed address.
/// Used by tests; real programs go through `spawn_elf`.
pub fn spawn_raw(code: &[u8]) -> Pid {
    const RAW_ENTRY: u64 = 0x40_0000;
    let mut space = VmSpace::new();
    let hhdm = crate::boot::hhdm_offset();
    let pa = crate::frames::alloc(0).expect("out of frames");
    unsafe {
        core::ptr::write_bytes((hhdm + pa) as *mut u8, 0, qunix_mm::PAGE_SIZE as usize);
        core::ptr::copy_nonoverlapping(code.as_ptr(), (hhdm + pa) as *mut u8, code.len());
    }
    space.map_user(RAW_ENTRY, pa, false, true);

    let process = create(RAW_ENTRY, space);
    start(process)
}

pub fn spawn_elf(image: &[u8]) -> Result<Pid, ElfError> {
    let mut space = VmSpace::new();
    let entry = crate::loader::load(&mut space, image)?;
    let process = create(entry, space);
    Ok(start(process))
}

fn start(process: Arc<Process>) -> Pid {
    let pid = process.pid;
    let raw = Arc::into_raw(process) as u64;
    crate::sched::spawn_user(user_thread_entry, raw, Priority::Normal);
    pid
}

/// First code a user thread runs, still in ring 0. Activates the process's
/// address space, then drops to ring 3 and never returns.
extern "C" fn user_thread_entry(arg: u64) -> ! {
    let process = unsafe { Arc::from_raw(arg as *const Process) };
    let entry = process.entry;
    let stack = process.user_stack_top;

    crate::sched::set_current_process(Arc::clone(&process));
    unsafe { process.space.lock().activate() };
    // Keep the process alive for the life of the thread.
    core::mem::forget(process);

    unsafe { qunix_hal_x86_64::syscall::enter_user(entry, stack) };
}

pub fn current_pid() -> Pid {
    crate::sched::current_process().map(|p| p.pid).unwrap_or(Pid(0))
}
```

Add to `sched.rs`:

```rust
// kernel/src/sched.rs  (append)
use alloc::sync::Arc;

/// Same as `spawn_kernel`, but the thread is expected to drop to ring 3.
/// Separate so the distinction is visible at the call site.
pub fn spawn_user(entry: extern "C" fn(u64) -> !, arg: u64, prio: Priority) -> ThreadId {
    spawn_kernel(entry, arg, prio)
}

pub fn set_current_process(process: Arc<crate::process::Process>) {
    let mut s = SCHED.lock();
    let id = s.current.expect("sched::init not called");
    s.threads.get_mut(&id).unwrap().process = Some(process);
}

pub fn current_process() -> Option<Arc<crate::process::Process>> {
    let s = SCHED.lock();
    let id = s.current?;
    s.threads.get(&id)?.process.clone()
}
```

- [ ] **Step 6: Register the modules and run the full suite**

```rust
// kernel/src/main.rs
mod loader;
mod process;
mod vmspace;
```

Add `qunix-elf.workspace = true` and `qunix-abi.workspace = true` to `kernel/Cargo.toml` and to `[workspace.dependencies]`.

Also install the syscall ABI in `kmain`:
```rust
// kernel/src/main.rs  (in kmain, after sched::init)
    syscall::init();
```

Run: `cargo xtask test`
Expected: PASS — including the Task 8 ring-3 test, which now has `spawn_raw`.

If the machine triple-faults on entering ring 3, check in this order: the TSS `privilege_stack_table[0]` is set for the running thread (Task 4), `IA32_STAR` matches the GDT selector order (Task 1 asserts it), and the entry page is mapped with `USER` set and `NO_EXECUTE` clear.

- [ ] **Step 7: Commit**

```bash
git add kernel Cargo.toml
git commit -m "feat(proc): processes, elf loading, and ring-3 entry"
```

---

### Task 11: Launch a Real Init Binary

**Files:**
- Create: `userspace/init/Cargo.toml`, `userspace/init/src/main.rs`, `targets/x86_64-qunix-user.json`
- Modify: `Cargo.toml`, `xtask/src/main.rs`, `xtask/src/image.rs`, `limine.conf`, `kernel/src/main.rs`, `kernel/src/boot.rs`

**Interfaces:**
- Consumes: `qunix_abi::Sys`, Limine module request.
- Produces:
  - A freestanding `init` binary for `x86_64-qunix-user`
  - `boot::module(name: &str) -> Option<&'static [u8]>`
  - `kmain` spawning `init` from a boot module

- [ ] **Step 1: Add a userspace target**

```json
{
  "llvm-target": "x86_64-unknown-none",
  "data-layout": "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128",
  "arch": "x86_64",
  "target-endian": "little",
  "target-pointer-width": "64",
  "target-c-int-width": "32",
  "os": "none",
  "vendor": "unknown",
  "executables": true,
  "linker-flavor": "gnu-lld",
  "linker": "rust-lld",
  "panic-strategy": "abort",
  "relocation-model": "static",
  "static-position-independent-executables": false
}
```

This differs from the kernel target in three ways, all deliberate: the red zone stays enabled, SSE stays enabled, and the code model is the default `small` — userspace lives in the lower half, so the kernel code model would be wrong.

- [ ] **Step 2: Write the init binary**

```rust
// userspace/init/src/main.rs
#![no_std]
#![no_main]

use core::arch::asm;

#[inline(always)]
unsafe fn syscall1(nr: u64, a0: u64) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") a0,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // Prove we are alive, then prove getpid works, then exit.
    unsafe { syscall1(qunix_abi::Sys::Write as u64, 0xC0DE) };
    let pid = unsafe { syscall1(qunix_abi::Sys::GetPid as u64, 0) };
    unsafe { syscall1(qunix_abi::Sys::Write as u64, pid as u64) };
    unsafe { syscall1(qunix_abi::Sys::Exit as u64, 0) };
    unreachable!()
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { syscall1(qunix_abi::Sys::Exit as u64, 1) };
    unreachable!()
}
```

```toml
# userspace/init/Cargo.toml
[package]
name = "init"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
qunix-abi.workspace = true
```

Add `"userspace/*"` to the workspace `members`.

- [ ] **Step 3: Build init and ship it as a boot module**

```rust
// xtask/src/main.rs  (add before build_kernel is used by run/test)
fn build_init(root: &Path) -> Result<PathBuf> {
    let mut cmd = Command::new(env!("CARGO"));
    cmd.current_dir(root);
    cmd.args([
        "build", "--package", "init",
        "--target", "targets/x86_64-qunix-user.json",
    ]);
    if !cmd.status()?.success() {
        bail!("init build failed");
    }
    Ok(root.join("target/x86_64-qunix-user/debug/init"))
}
```

```rust
// xtask/src/image.rs  (in build_iso, after copying the kernel)
    let init = root.join("target/x86_64-qunix-user/debug/init");
    std::fs::copy(&init, iso_root.join("boot/init"))
        .context("copying the init binary into the image")?;
```

```
# limine.conf
timeout: 0

/qunix
    protocol: limine
    kernel_path: boot():/boot/qunix-kernel
    module_path: boot():/boot/init
```

Call `build_init` from the `run`, `test`, and `runner` arms before `build_iso`.

- [ ] **Step 4: Write the failing test**

```rust
// kernel/src/main.rs  (inside mod tests)
    #[test_case]
    fn the_init_module_is_present_and_is_a_valid_elf() {
        let module = crate::boot::module("init").expect("init module missing from the boot image");
        assert!(module.len() > 64);
        assert_eq!(&module[0..4], &[0x7f, b'E', b'L', b'F']);
        qunix_elf::Elf64::parse(module).expect("init is not a loadable ELF");
    }

    #[test_case]
    fn init_runs_in_ring_three_and_reports_its_pid() {
        crate::boot_prelude();
        crate::sched::init();
        crate::syscall::init();

        let module = crate::boot::module("init").unwrap();
        let pid = crate::process::spawn_elf(module).expect("spawning init failed");

        let mut budget = 200_000;
        while crate::syscall::test_probe_value() != pid.0 && budget > 0 {
            crate::sched::yield_now();
            budget -= 1;
        }
        assert!(budget > 0, "init never reported its pid");
    }
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo xtask test`
Expected: FAIL — `cannot find function module`.

- [ ] **Step 6: Add the module request**

```rust
// kernel/src/boot.rs  (append)
use limine::request::ModulesRespData;

#[used]
#[unsafe(link_section = ".requests")]
pub static MODULES: Request<ModulesRespData> = Request::new();

/// Returns a boot module by the trailing component of its path.
pub fn module(name: &str) -> Option<&'static [u8]> {
    let response = MODULES.response()?;
    for file in response.modules() {
        let path = file.path().to_str().ok()?;
        if path.rsplit('/').next() == Some(name) {
            return Some(unsafe {
                core::slice::from_raw_parts(file.addr(), file.size() as usize)
            });
        }
    }
    None
}
```

Verify `modules()`, `path()`, `addr()`, and `size()` against the generated docs for `limine` 0.6.5 exactly as in Task 6 Step 5, and correct any name that differs before continuing.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo xtask test`
Expected: PASS.

- [ ] **Step 8: Launch init from `kmain`**

```rust
// kernel/src/main.rs  (in kmain, after smp::start_all)
    match boot::module("init") {
        Some(image) => match process::spawn_elf(image) {
            Ok(pid) => println!("qunix: started init as pid {}", pid.0),
            Err(e) => println!("qunix: init failed to load: {e:?}"),
        },
        None => println!("qunix: no init module in the boot image"),
    }
    sched::set_preemption(true);
    x86_64::instructions::interrupts::enable();
    loop {
        sched::yield_now();
        x86_64::instructions::hlt();
    }
```

- [ ] **Step 9: Verify the real boot**

Run: `cargo xtask run --bios`
Expected serial output, in order:
```
qunix: booted
qunix: gdt installed
qunix: idt installed
qunix: hhdm at 0x..., ... MiB usable
qunix: ... MiB of frames available
qunix: kernel heap online
qunix: apic timer running
qunix: 4 cpus online
qunix: started init as pid 1
user write: 49374
user write: 1
```
`49374` is `0xC0DE`. Terminate QEMU with `Ctrl-A X`.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml limine.conf targets userspace kernel xtask
git commit -m "feat(init): load and run a real userspace init binary"
```

---

## Milestone Exit Criteria

M1 is complete when all of the following hold:

- [ ] No `static mut` remains anywhere in the kernel or HAL crates (`grep -rn 'static mut' kernel crates` returns nothing).
- [ ] `cargo xtask test` passes, covering: per-CPU state, run-queue policy, context switching, cooperative scheduling, timer preemption, SMP bring-up, address-space isolation, ELF parsing and loading, and end-to-end ring-3 execution.
- [ ] All four QEMU CPUs come online.
- [ ] A kernel thread that never yields is still preempted by the timer.
- [ ] A user mapping in one address space is not visible in another.
- [ ] `cargo xtask run` boots and runs a real `init` ELF in ring 3, which issues syscalls and exits cleanly.

### D4 — SYSCALL argument registers do not line up with System V (Task 8, 2026-08-05)

The plan's stub moves `r10` into `rcx` and calls the handler, implying the rest
of the registers already match. They do not:

    syscall:  nr=rax  a0=rdi  a1=rsi  a2=rdx  a3=r10  a4=r8
    sysv:     nr=rdi  a0=rsi  a1=rdx  a2=rcx  a3=r8   a4=r9

Every argument shifts by one register, and the syscall number has to move from
`rax` into `rdi`. The first version of the stub did only the `r10` move, and
the failure was silent: the kernel dispatched on whatever was in `rdi`, so a
process calling `exit(7)` had its message *address* interpreted as the syscall
number, both syscalls returned `BadSyscall`, and execution ran off the end of
the program into a `ud2`. Nothing faulted at the point of the mistake.

The moves are written right-to-left so each source is read before it is
overwritten.

### D5 — `TSS.rsp0` is a second, separate kernel stack pointer (Task 8/10, 2026-08-05)

`percpu::kernel_rsp` is read by the `SYSCALL` stub, which switches stacks
itself because `syscall` does not. `TSS.privilege_stack_table[0]` is read by
the *CPU* on any interrupt or exception taken from ring 3. They are different
mechanisms and both must be set; the plan mentions only the first.

Setting only `kernel_rsp` produces a kernel that services syscalls correctly
and then dies on the first timer tick that lands while a process is running,
because the CPU pushes the interrupt frame to address 0. `percpu::set_kernel_stack`
now sets both, which is why it lives there rather than in `syscall`.

## Known Limitations Carried Into M2

- **Application processors are online but idle.** They install per-CPU state
  and park. Making them schedule requires moving `sched::Scheduler::current`
  and the run queue into `percpu::PerCpu`; see Execution Deviation D3.


1. **Application processors idle.** `smp::start_all` brings APs online but leaves them halted; they have no run queues. Per-CPU scheduling and work stealing use the `RunQueue::steal` already implemented here.
2. **No TLB shootdown.** Unmapping still flushes only the local CPU. Now that there is more than one CPU, this is a live correctness bug rather than a theoretical one — it must be fixed before any address space is modified while shared.
3. **Address spaces leak intermediate page tables.** `VmSpace::drop` reclaims only the PML4 frame.
4. **No process reaping.** Exited threads are removed from the scheduler, but `PROCESSES` grows without bound.
5. **`sys_write` takes a value, not a buffer.** There are no file descriptors until the M2 VFS exists; the syscall is a placeholder that proves the ABI path works.
6. **No user-pointer validation beyond a range check.** Copying to and from userspace needs fault-tolerant accessors before any syscall accepts a real buffer.
7. **Single global run queue behind one lock.** Correct but not scalable; replaced by per-CPU queues in M2.
