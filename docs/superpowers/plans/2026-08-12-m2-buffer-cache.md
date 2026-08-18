# M2 T5 — Buffer Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A page-granular buffer cache over virtio-blk, keyed by `(dev, block)`, with dirty tracking and writeback that never calls the general allocator.

**Architecture:** The pure logic — the slot table, its keying, its state machine and its eviction policy — lives in a host-tested `no_std` crate with no I/O and no allocator. The kernel half owns a pool of frames reserved at init, and drives `block::read_at`/`write_at` to fill and flush them. This is the same split as `qunix-virtio` / `kernel::virtio`, and for the same reason: the failure mode here is a *wrong answer* — a slot returning another block's bytes — which is an assertion about arithmetic, and arithmetic is testable on the host.

**Tech Stack:** Rust 2024, `no_std`, `qunix-virtio`-style pure crate + kernel glue, the async runtime from T3, the block driver from T4.

## Global Constraints

Every task's requirements implicitly include these. Values are copied from the
spec and from `CLAUDE.md`.

- **`no_std`** in both new crate and kernel code. No `std`, no floating point.
- **The writeback path must not call the general allocator.** From the spec's
  Risks: "The buffer cache must be able to write back without calling the
  general allocator." Memory pressure triggers writeback, writeback needs I/O,
  I/O needs the allocation that is already waiting. Every buffer, every slot and
  every future on the flush path comes from storage reserved at init.
- **No `dyn` in the I/O path.** The spec rejects `dyn Vnode` because
  `async fn` in a trait needs `Pin<Box<dyn Future>>`, which is an allocation per
  read. The cache's futures must stay concrete state machines.
- **Block size is 4096 bytes**, one page and exactly `block::MAX_SECTORS`
  sectors. The block driver refuses anything larger, so a cache block is the
  largest transfer the device layer will carry in one request.
- **Edition 2024 unsafe attributes**: `#[unsafe(no_mangle)]`,
  `#[unsafe(link_section = "…")]`.
- **Licensing:** permissive, `qunix-*` zone. This is clean-roomed from the
  design above, not from any kernel's source.
- **Every commit leaves `cargo xtask test` green**, both boots.

---

## File Structure

| Path | Responsibility |
| --- | --- |
| `crates/qunix-bcache/Cargo.toml` | New workspace member, permissive licence, `std` feature for host tests |
| `crates/qunix-bcache/src/lib.rs` | `BlockKey`, `SlotState`, `Cache` — the slot table, its lookup, its state machine and its eviction choice. No I/O, no allocation. |
| `kernel/src/bcache.rs` | The frames reserved at init, and the async fill/flush that drives `crate::block` |
| `kernel/src/main.rs` | `mod bcache;` and its init call |
| `Cargo.toml` | Workspace member and dependency entry |

The split is where the reasoning lives: the crate answers "which slot, and may
it be reused", the kernel answers "what is in it".

---

### Task 1: The slot table and its keying

**Files:**
- Create: `crates/qunix-bcache/Cargo.toml`, `crates/qunix-bcache/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members` and `[workspace.dependencies]`)
- Test: in-crate `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `BlockKey { dev: u32, block: u64 }`, `Cache::new(capacity)`,
  `Cache::lookup(key) -> Option<usize>`, `Cache::CAPACITY`.
- Consumes: nothing.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn two_devices_with_the_same_block_number_are_different_blocks() {
    // The key is a pair, and collapsing it to the block number alone returns
    // one device's data for another's -- a wrong answer, reported as a hit.
    let mut cache = Cache::new(4);
    let a = cache.insert(BlockKey { dev: 0, block: 7 }).expect("a fresh cache has slots");
    let b = cache.insert(BlockKey { dev: 1, block: 7 }).expect("a fresh cache has slots");
    assert_ne!(a, b, "two devices' block 7 landed in one slot");
    assert_eq!(cache.lookup(BlockKey { dev: 0, block: 7 }), Some(a));
    assert_eq!(cache.lookup(BlockKey { dev: 1, block: 7 }), Some(b));
}

#[test]
fn a_block_that_was_never_inserted_is_a_miss() {
    // The direction that matters: a spurious *hit* hands out a slot holding
    // some other block's bytes, and nothing downstream re-checks.
    let cache = Cache::new(4);
    assert_eq!(cache.lookup(BlockKey { dev: 0, block: 0 }), None);
}
```

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p qunix-bcache --features std --target x86_64-unknown-linux-musl`
Expected: FAIL — `Cache` does not exist.

- [ ] **Step 3: Implement**

```rust
#![cfg_attr(not(any(test, feature = "std")), no_std)]

/// Which block, on which device.
///
/// A pair, not a block number: two devices' block 7 are different blocks, and
/// keying on the number alone returns one device's bytes for the other's --
/// a wrong answer reported as a cache hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockKey {
    pub dev: u32,
    pub block: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// Holds no block.
    Free,
    /// Contents match the device.
    Clean,
    /// Contents differ from the device and must be written before reuse.
    Dirty,
    /// An I/O is outstanding against this slot.
    InFlight,
}

pub struct Slot {
    key: BlockKey,
    state: SlotState,
    /// Callers currently holding this slot.
    pins: u32,
}

pub struct Cache {
    slots: [Slot; Self::CAPACITY],
    used: usize,
}
```

Use a fixed array, not a `Vec`: the writeback path may not allocate, and a
table that can grow is a table that can allocate while flushing.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p qunix-bcache --features std --target x86_64-unknown-linux-musl`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/qunix-bcache
git commit -m "feat(bcache): the slot table, keyed by device and block"
```

---

### Task 2: The state machine, and what may not be evicted

**Files:**
- Modify: `crates/qunix-bcache/src/lib.rs`
- Test: same module

**Interfaces:**
- Produces: `Cache::mark_dirty(slot)`, `Cache::mark_clean(slot)`,
  `Cache::begin_io(slot)`, `Cache::end_io(slot)`, `Cache::pin`/`unpin`,
  `Cache::victim() -> Option<usize>`, `Cache::dirty_slots()`.

This is the task that carries the milestone's risk, so its tests are written in
the refusing direction first.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_dirty_slot_is_never_chosen_as_a_victim() {
    // Evicting a dirty slot silently discards a write the caller believes
    // succeeded. Nothing downstream can notice: the block simply has its old
    // contents the next time it is read.
    let mut cache = Cache::new(2);
    let a = cache.insert(BlockKey { dev: 0, block: 1 }).unwrap();
    let b = cache.insert(BlockKey { dev: 0, block: 2 }).unwrap();
    cache.mark_dirty(a);
    cache.mark_dirty(b);
    assert_eq!(cache.victim(), None, "a dirty slot was offered for reuse");
}

#[test]
fn a_slot_with_io_outstanding_is_never_chosen_as_a_victim() {
    // The device owns the buffer until it completes. Reusing it here is the
    // driver's own bug one layer up: the device writes one block's data into
    // another block's buffer, and every operation returns success.
    let mut cache = Cache::new(2);
    let a = cache.insert(BlockKey { dev: 0, block: 1 }).unwrap();
    cache.begin_io(a);
    assert_eq!(cache.victim(), Some(1), "the only reusable slot was not offered");
    let b = cache.insert(BlockKey { dev: 0, block: 2 }).unwrap();
    cache.begin_io(b);
    assert_eq!(cache.victim(), None, "a slot with io outstanding was offered for reuse");
}

#[test]
fn a_pinned_slot_is_never_chosen_as_a_victim() {
    // A pin means somebody holds a reference to the buffer.
    let mut cache = Cache::new(2);
    let a = cache.insert(BlockKey { dev: 0, block: 1 }).unwrap();
    cache.pin(a);
    let b = cache.insert(BlockKey { dev: 0, block: 2 }).unwrap();
    cache.pin(b);
    assert_eq!(cache.victim(), None, "a pinned slot was offered for reuse");
}

#[test]
fn every_dirty_slot_is_reported_for_writeback() {
    // `sync` writes what this reports. A dirty slot missing from it is a write
    // that is acknowledged and never reaches the disk.
    let mut cache = Cache::new(4);
    let a = cache.insert(BlockKey { dev: 0, block: 1 }).unwrap();
    let b = cache.insert(BlockKey { dev: 0, block: 2 }).unwrap();
    cache.insert(BlockKey { dev: 0, block: 3 }).unwrap();
    cache.mark_dirty(a);
    cache.mark_dirty(b);
    let mut reported = cache.dirty_slots().collect::<alloc::vec::Vec<_>>();
    reported.sort_unstable();
    assert_eq!(reported, alloc::vec![a, b], "the dirty set does not match what was dirtied");
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p qunix-bcache --features std --target x86_64-unknown-linux-musl`
Expected: FAIL — the methods do not exist.

- [ ] **Step 3: Implement**

`victim` returns a slot only when `state` is `Free` or `Clean` **and** `pins == 0`.
Record why in a comment at the guard: each of the three refusals corresponds to
a different corruption, and they are not interchangeable.

- [ ] **Step 4: Run the tests**

Expected: PASS

- [ ] **Step 5: Mutation-check the guard**

Delete the `Dirty` arm of the refusal, re-run, and require
`a_dirty_slot_is_never_chosen_as_a_victim` to fail. Restore it. Repeat for the
`InFlight` and pin arms. A guard that no test can falsify is not guarded.

- [ ] **Step 6: Commit**

```bash
git add crates/qunix-bcache/src/lib.rs
git commit -m "feat(bcache): the slot state machine, and the three things it refuses to evict"
```

---

### Task 3: Frames reserved at init, and a cached read

**Files:**
- Create: `kernel/src/bcache.rs`
- Modify: `kernel/src/main.rs` (`mod bcache;`)

**Interfaces:**
- Consumes: `qunix_bcache::{BlockKey, Cache}`, `crate::block::{read_at, MAX_SECTORS}`,
  `crate::frames`, `crate::boot::hhdm_offset`.
- Produces: `pub async fn read_block(key: BlockKey) -> Result<&'static [u8], BcacheError>`,
  `pub fn init()`.

- [ ] **Step 1: Reserve the frames at init**

One frame per slot, allocated once in `init` and never returned. Say so at the
allocation: this is the whole reason the writeback path cannot allocate, and a
future reader who "fixes" it into an on-demand allocation reintroduces the
deadlock the spec names.

- [ ] **Step 2: Write the failing in-QEMU test**

```rust
#[test_case]
fn a_cached_read_returns_the_bytes_the_disk_holds() {
    // The test disk's every sector begins with its own LBA, so a slot holding
    // the wrong block is visible in the data rather than only in the bookkeeping.
    crate::bcache::init();
    let block = block_on(crate::bcache::read_block(BlockKey { dev: 0, block: 1 }))
        .expect("a cached read of a live block failed");
    // Block 1 is sectors 8..16, so the first eight bytes are LBA 8.
    assert_eq!(u64::from_le_bytes(block[0..8].try_into().unwrap()), 8);
}

#[test_case]
fn a_second_read_of_the_same_block_does_no_further_io() {
    // The point of the cache. Asserted against the driver's own counter rather
    // than against timing, which would pass on any machine slow enough.
    crate::bcache::init();
    let key = BlockKey { dev: 0, block: 2 };
    block_on(crate::bcache::read_block(key)).expect("the first read failed");
    let completions = crate::block::completions();
    block_on(crate::bcache::read_block(key)).expect("the second read failed");
    assert_eq!(
        crate::block::completions(),
        completions,
        "a cache hit still went to the device"
    );
}
```

- [ ] **Step 3: Run and watch them fail**

Run: `cargo xtask test`
Expected: FAIL — `bcache` does not exist.

- [ ] **Step 4: Implement `read_block`**

Look up; on a hit, return the slot. On a miss, take a victim, `begin_io`, await
`block::read_at` into that slot's frame, `end_io`, mark `Clean`.

- [ ] **Step 5: Run the tests**

Run: `cargo xtask test`
Expected: PASS, both boots.

- [ ] **Step 6: Commit**

```bash
git add kernel/src/bcache.rs kernel/src/main.rs
git commit -m "feat(bcache): cached reads over frames reserved at init"
```

---

### Task 4: Dirty tracking and writeback

**Files:**
- Modify: `kernel/src/bcache.rs`

**Interfaces:**
- Produces: `pub async fn write_block(key, &[u8]) -> Result<(), BcacheError>`,
  `pub async fn sync() -> Result<(), BcacheError>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test_case]
fn a_written_block_reaches_the_disk_only_after_sync() {
    // Both halves. A write that reaches the disk immediately is not a cache;
    // a write that never reaches it is data loss, and the acknowledgement the
    // caller already has makes it silent.
    crate::bcache::init();
    let key = BlockKey { dev: 0, block: 3 };
    let mut payload = alloc::vec![0u8; BLOCK_BYTES];
    payload[0..8].copy_from_slice(&0xfeed_face_u64.to_le_bytes());

    block_on(crate::bcache::write_block(key, &payload)).expect("the write was refused");
    let before = read_through_the_device(key);
    assert_ne!(&before[0..8], &payload[0..8], "the write reached the disk before sync");

    block_on(crate::bcache::sync()).expect("sync failed");
    let after = read_through_the_device(key);
    assert_eq!(&after[0..8], &payload[0..8], "sync did not write the block back");
}
```

`read_through_the_device` bypasses the cache with `block::read_at` into a local
buffer, because reading through the cache would return the dirty slot and
assert nothing about the disk.

- [ ] **Step 2: Run and watch it fail**

Run: `cargo xtask test`
Expected: FAIL — `write_block` does not exist.

- [ ] **Step 3: Implement**

`write_block` fills the slot and marks it `Dirty`. `sync` walks `dirty_slots`,
`begin_io`, awaits `block::write_at`, `end_io`, marks `Clean`.

- [ ] **Step 4: Run the tests**

Run: `cargo xtask test`
Expected: PASS, both boots.

- [ ] **Step 5: Commit**

```bash
git add kernel/src/bcache.rs
git commit -m "feat(bcache): dirty tracking and writeback"
```

---

### Task 5: Eviction under pressure

**Files:**
- Modify: `kernel/src/bcache.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test_case]
fn evicting_a_dirty_slot_writes_it_back_first() {
    // The failure this exists to prevent is silent: the slot is reused, the
    // write is discarded, and the block reads as its old contents forever.
    crate::bcache::init();
    let victim = BlockKey { dev: 0, block: 4 };
    let mut payload = alloc::vec![0u8; BLOCK_BYTES];
    payload[0..8].copy_from_slice(&0x0bad_cafe_u64.to_le_bytes());
    block_on(crate::bcache::write_block(victim, &payload)).expect("the write was refused");

    // Read enough distinct blocks to force every slot to turn over.
    for block in 100..(100 + CAPACITY as u64 + 1) {
        block_on(crate::bcache::read_block(BlockKey { dev: 0, block }))
            .expect("a read failed while filling the cache");
    }

    let on_disk = read_through_the_device(victim);
    assert_eq!(
        &on_disk[0..8],
        &payload[0..8],
        "a dirty slot was evicted without being written back"
    );
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo xtask test`
Expected: FAIL — the dirty block's old contents are still on disk.

- [ ] **Step 3: Implement**

When `victim()` returns `None` because every reusable slot is dirty, flush
before retrying. The flush uses the same reserved frames and the same concrete
futures — no allocation on this path, which is the constraint the whole design
exists to satisfy.

- [ ] **Step 4: Run the tests**

Run: `cargo xtask test`
Expected: PASS, both boots.

- [ ] **Step 5: Verify the allocator is not on the path**

Read `read_block`, `write_block`, `sync` and the eviction path and confirm no
`Box`, `Vec`, `format!` or `alloc::` call appears in any of them. This is a
review step rather than an assertion because the check is "no allocation
reachable", which no single test can state — say what was checked in the commit
message.

- [ ] **Step 6: Commit**

```bash
git add kernel/src/bcache.rs
git commit -m "feat(bcache): write a dirty slot back before reusing it"
```

---

## Execution Deviations

Recorded during execution. A plan written before the code is a hypothesis; the
deviations are the result.

### D1: the capacity is a const parameter, not a constructor argument

The plan writes `Cache::new(capacity)` with a runtime capacity. That needs
storage sized at runtime, which means a `Vec`, which means the flush path can
allocate -- and the whole reason the table is fixed is that memory pressure
triggers writeback, writeback needs I/O, and I/O needs the allocation already
waiting.

`Cache<const N: usize>` removes the possibility rather than documenting it.
There is no `Vec` to grow and nowhere for one to appear later without the type
changing. `Cache::CAPACITY` replaces the `capacity()` accessor the plan
assumed.

### D2: two crates were outside the coverage ratchet

Not a change to the plan, but found while adding `qunix-bcache` to it:
`xtask`'s `MEASURED` list is hand-written and `qunix-virtio` was never added
after M2 T4, so the crate had no floor for a whole milestone. Nothing reported
it -- the run prints success for the crates it measured and is silent about the
one it skipped.

`every_host_testable_crate_is_measured` now fails when a crate with a `std`
feature is missing from the list. Floors: `qunix-bcache` 100.00%,
`qunix-virtio` 98.37%.

Separately, `qunix-elf` measures 99.76% against a committed floor of 100.00%.
`TOLERANCE_PP` absorbs that on *read*, so the ratchet passes, but
`--update` refuses to write the lower figure without a stated reason -- which
is why the two new floors above were written by hand rather than by
`--update`. The elf gap is pre-existing and untouched here.

### D3: the review found the claimed slot was marked clean

`/code-review` on Task 2 found that `insert` marked a freshly claimed slot
`Clean` — "these contents match the device" — before anything had been read
into it. The plan's Task 3 read path is `lookup` miss → `insert` → `await`, so
a second caller looking the key up across that await took the hit and read a
buffer holding whatever the frame held before, with every operation returning
success. That is the exact failure the crate's module doc is written against,
and Task 2's tests did not catch it because they only ever inserted and then
used the slot from the same thread.

A claimed slot is now `InFlight` and `end_io` is what makes it readable.
Seven further findings from the same review are fixed in the same commit;
`evict` is new, because `victim` named a slot and nothing public could act on
the name, so as written the cache could not evict at all.

### D4: two hand-written crate lists, both stale

`qunix-bcache` was in neither `xtask test`'s crate list nor the coverage
ratchet's, so its tests never ran under `cargo xtask test` and it had no
floor — and the run reported success either way, because nothing in the
output names a suite that did not run. `qunix-abi` was missing from both too.

There is now one `HOST_CRATES` list read by both, and two tests that fail when
a crate is missing from it. The first attempt at that check used "has a `std`
feature" as the signal and passed `qunix-abi`, reproducing the gap inside the
check written to close it; membership is the workspace directory now.

### D5: `read_block` returns a pin, not `&'static [u8]`

The plan has `read_block` return `&'static [u8]`. Task 5 adds eviction under
pressure, and a bare reference into a slot stays valid-*looking* after that slot
is evicted and refilled: the read succeeds and the bytes are another block's,
which is the failure this whole crate is written against.

It returns a `BlockRef` instead — a guard that pins its slot on creation and
unpins on drop. That is also what gives `pin`/`unpin` a caller; without it they
were an API nothing used.
`a_pinned_block_is_not_evicted_out_from_under_its_reader` fails when eviction
ignores pins, and `a_table_of_pinned_blocks_reports_exhaustion_rather_than_evicting_one`
asserts the refusal rather than the success.

### D6: a hit on a slot with I/O outstanding is not always a wait

The plan's read path treats `InFlight` as one state. It is two: a slot being
*filled* holds nothing yet and a reader must wait, while a slot being *written
back* holds the caller's own data and the device is only reading it, so a reader
may proceed. Collapsing them either serves an unfilled buffer or stalls every
reader behind every flush, and the second is invisible — it costs latency, not
correctness. `Bcache::filling` carries the distinction, which Task 4's writeback
needs before it exists.

A second reader that does have to wait retries on a bounded timer rather than
joining a waiter list. A per-slot waiter list is unbounded state in a table
whose fixed size is the reason the flush path cannot allocate, and the thing
being waited for is a disk read. The bound (`FILL_WAIT_LIMIT`) is what stops a
lost completion becoming a stopped machine instead of a failed request.

### D7: the test disk persists, so the plan's writeback test cannot work

Task 4's test as written asserts `assert_ne!(&before[0..8], &payload[0..8], "the
write reached the disk before sync")` against a fixed payload. But
`xtask::image::build_test_disk` writes the image *only when absent*, deliberately
— its own doc says regenerating per run "would erase whatever a write test had
just put there". So the first boot leaves `0xfeed_face` on block 3 and the second
boot's copy of the same test finds it already there and fails the `assert_ne`.
`cargo xtask test` runs the suite twice, so this would have failed on its first
green run.

The test derives its payload from whatever the disk currently holds (every byte
inverted), asserts both halves against that, and then restores the original and
asserts the restore. It is repeatable from any starting state and leaves the
image as it found it. The write tests own blocks 240..248 so a payload left
behind by a failure cannot change what another test reads.

### D8: `write_block` refuses a partial block, and issues no read

A full-block write has no need to read the block it replaces, so `write_block`
does no I/O at all: it copies under the lock and marks the slot dirty. Which
means a write that does *not* cover the block cannot be served — padding invents
bytes the caller never supplied and puts them on the disk, and merging is a
read-modify-write, a different operation with a different failure mode.
`BcacheError::PartialBlock` refuses it, with a test.

`store` waits on *any* outstanding I/O, not just a fill. Task 3's read path
proceeds through a writeback because the device is only reading the buffer; a
writer must not, because it would hand the device a torn mixture of the block it
was told to write and the one written over it — successfully.

### D9: the whole-block assertion was not whole-block

Mutation-testing the writeback length found the test suite passing with `sync`
sending one sector instead of eight. The payload helper inverted only the first
sector, so the remaining seven equalled what was already on the disk and the
`assert_eq!(read_through_the_device(key), payload)` comparison was satisfied by a
one-sector write. It inverts every byte now.

This is the failure mode `CLAUDE.md` warns about — an assertion that reads as
comprehensive and is not — and it was invisible to every other check. The only
thing that surfaced it was mutating the length and finding the suite still green.

### D10: a pin excludes overwriting the buffer, not only evicting the slot

Review found `write_block` copying 4096 bytes over a frame while a `BlockRef`
handed out a `&[u8]` over the same bytes. `BlockRef::deref` deliberately does
*not* hold the lock — holding it would mask interrupts for as long as a caller
chose to look at a block — so the pin is the entire exclusion, and `store`
consulted only the slot's state. A reader walking a directory block would see a
mixture of two blocks, successfully, and it is also a write through a raw
pointer aliasing a live shared reference.

`Cache::pins_of` is new so the kernel can ask. A write now waits a short bound
for readers to leave and reports `BcacheError::Pinned` if they do not — short
because the thing being waited for is not I/O, and because the most likely cause
is a caller holding a `BlockRef` to the block it is writing and so waiting for
itself. `sync` is exempt: the device only reads the buffer, so it may share it.

`a_write_is_refused_while_a_reader_holds_the_block` asserts the reader's bytes
are unchanged, not merely that the call failed.

### D11: Task 5's test could not observe eviction at all

The plan's test writes one block, reads `CAPACITY + 1` distinct blocks to "force
every slot to turn over", and asserts the written block reached the disk. It
cannot: `victim` prefers a *clean* slot, and unpinned clean slots keep being
recycled, so the dirty one is never considered. Reading a thousand blocks would
leave it exactly where it was — the test would fail identically with and without
the feature, and for a reason unrelated to it.

Real pressure has to be constructed. `pin_every_slot_but_one` fills the table
and holds a `BlockRef` to every slot but the dirty one, so serving one more
distinct block *requires* writing that block back. That is also what the two
refusal tests need: `a_dirty_slot_the_device_refuses_is_not_evicted_anyway`
requires the read to report the device's error rather than hide a failing disk
behind `Exhausted`, and `a_write_under_pressure_also_flushes_rather_than_refusing`
requires the write path to answer pressure the same way the read path does.

### D12: the pin filter on the flush candidate is load-bearing

`flushable_slot` skips pinned slots. Mutation testing found that removing the
filter passed every test, because no test had a slot that was dirty *and*
pinned — the writes all happened to unpinned slots.

It is not cosmetic. Writing back a pinned block frees nothing: the pin is what
refuses the eviction and the writeback does not remove it. So each such choice
spends a disk write to make no room, and the retry bound (`SLOTS` attempts) then
runs out while a reusable slot was available all along — a table reporting
`Exhausted` with room in it.
`a_pinned_dirty_slot_is_not_the_one_chosen_to_flush` asserts the pinned block is
still dirty and still not on the disk, which is what states the choice.

