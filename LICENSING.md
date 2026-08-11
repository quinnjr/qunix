# Licensing

qunix is deliberately split into two licensing zones. Which zone a crate belongs
to is not a matter of taste — it follows from whether the crate implements a
Linux-derived API.

## Permissive zone — `MIT OR Apache-2.0`

Everything that is qunix's own design:

| Crate | |
| --- | --- |
| `qunix-abi` | native syscall numbers and host/kernel status contract |
| `qunix-sync` | spinlocks, IRQ-safe locks |
| `qunix-mm` | buddy frame allocator, size-class heap |
| `qunix-hal-x86_64` | GDT/IDT/APIC/paging/serial |
| `qunix-kernel` | the kernel binary |
| `xtask` | build and QEMU orchestration |

Texts: [`LICENSE-MIT`](LICENSE-MIT), [`LICENSE-APACHE`](LICENSE-APACHE).

## Copyleft zone — `GPL-2.0`

Anything that reimplements or shims the Linux driver API:

| Crate | Status |
| --- | --- |
| `qunix-linux-compat` | not yet written — milestone M6a |
| `linux-shim` | not yet written — milestone M6a |
| the Linux-compatible header set | not yet written — milestone M6a |

Text: [`LICENSE-GPL-2.0`](LICENSE-GPL-2.0).

**None of these exist yet.** The licence text is present so the boundary is
established before the first line of that code is written, not negotiated
afterwards.

### Why the split

Shipping Linux-API-compatible headers and a shim runtime puts that layer in
GPL-derivative-work territory, whatever the headers were typed from. Treating it
as GPL-2.0 from the outset is the conservative reading, and it keeps the kernel
core reusable by anyone who never touches the compatibility layer. This is
recorded in the design spec, §9.

Note the asymmetry that makes the split work: the *syscall personality*
(`qunix-linux-abi`, matching Linux UAPI struct layouts so unmodified binaries
run) is a different thing from the *driver API* (`qunix-linux-compat`,
reimplementing in-kernel interfaces). Only the latter is derivative in the sense
that matters here.

## Clean-room rule for the permissive zone

Outside the copyleft crates, every Linux-compatible interface is written from
the **specification**, never transcribed from Linux's implementation.

Permitted sources: published UAPI headers, `Documentation/`, on-disk and
on-wire format descriptions, `man` pages, and the standards an interface
implements. Not permitted: Linux's `.c` files, its internal headers, or a
transcription of either — including pasting kernel source into a language model
and keeping what comes back, and including reproducing from memory an algorithm
known specifically from having read that source.

The rule is about the provenance of the knowledge, not the resemblance of the
result. Two implementations written to one specification look alike, and a
format that must interoperate byte-for-byte has exactly one correct shape:
ext4's on-disk layout, `struct stat`'s field order and errno values are facts
about a format, and matching them is the point rather than a derivation.

Machine-generated code gets the same scrutiny and a little more suspicion. A
model can emit GPL source it was trained on without attribution, and fluency is
not evidence of independent derivation — if a generated block looks like it came
from somewhere, establish where before keeping it.

Where an interface genuinely cannot be implemented without reading the in-kernel
source, that is the signal it belongs in the copyleft zone. Move it there rather
than weakening this rule; the boundary is cheap to move and expensive to
relitigate.

Unenforceable by `xtask`, deliberately: no build check can see where knowledge
came from. It is a rule for people and for review, which is why it is written
here rather than implied.

## Enforcement

Every crate today declares `license.workspace = true`, which resolves to
`MIT OR Apache-2.0`. That is the correct default for the permissive zone and
exactly the wrong one for the copyleft zone — a new `qunix-linux-compat` would
inherit a permissive licence by doing nothing at all.

`cargo xtask test` therefore runs a check that fails the build if any crate whose
name matches the copyleft zone is not declared `GPL-2.0`, and if any other crate
drifts off `MIT OR Apache-2.0`. See `xtask/src/licensing.rs`.
