#![cfg_attr(not(any(test, feature = "std")), no_std)]

//! ELF64 parsing, enough to load a static executable.
//!
//! Parses a byte slice in place. No allocation, no I/O, and no trait
//! implementations that could run arbitrary code, so this is host-testable and
//! the kernel gets exactly one thing from it: a list of segments to map.
//!
//! # Everything here is untrusted input
//!
//! The bytes come from a file the kernel did not write. Every field that is
//! used as an offset or a length is checked against the buffer before it is
//! used, and every arithmetic combination of two such fields is checked for
//! overflow. A parser that trusts a header reads out of bounds on a truncated
//! or hostile file, which in a kernel is not a panic but a fault with no
//! handler.
//!
//! Deliberately not supported: dynamic linking, relocations, interpreters,
//! section headers. A static `ET_EXEC` is the whole target. Anything else is
//! refused by name rather than partially handled.

/// Why a file could not be parsed.
///
/// Distinct variants rather than one `Invalid`, because these are the errors a
/// user sees when their program will not run, and "bad magic" and "not x86-64"
/// send them to very different places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Fewer bytes than the structure being read requires.
    TooShort,
    BadMagic,
    /// Not 64-bit, not little-endian, or not the current ELF version.
    NotElf64,
    NotX86_64,
    /// Not `ET_EXEC`. A `ET_DYN` (PIE) binary needs relocation this does not do.
    NotExecutable,
    /// A program header names a range outside the file, or one whose bounds
    /// overflow.
    BadProgramHeader,
    /// `p_filesz` exceeds `p_memsz`, so the segment's file image does not fit
    /// in the memory it declares.
    SegmentTooLarge,
}

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;

const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// Byte offsets into the ELF64 header. Named rather than inlined because a
/// wrong constant here reads a plausible value from the wrong field, which is
/// far harder to spot than a parse failure.
mod ehdr {
    pub const TYPE: usize = 16;
    pub const MACHINE: usize = 18;
    pub const ENTRY: usize = 24;
    pub const PHOFF: usize = 32;
    pub const PHENTSIZE: usize = 54;
    pub const PHNUM: usize = 56;
    pub const SIZE: usize = 64;
}

/// Byte offsets into an ELF64 program header.
mod phdr {
    pub const TYPE: usize = 0;
    pub const FLAGS: usize = 4;
    pub const OFFSET: usize = 8;
    pub const VADDR: usize = 16;
    pub const FILESZ: usize = 32;
    pub const MEMSZ: usize = 40;
    pub const SIZE: usize = 56;
}

/// One `PT_LOAD` segment: what to map, where, and with which permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment<'a> {
    pub vaddr: u64,
    /// Bytes to reserve at `vaddr`. For a `Segment` from [`Elf64::segments`]
    /// this is always at least `data.len()` -- `parse` refuses a file where it
    /// is not -- and the difference is `.bss`, which the loader must zero.
    pub mem_size: u64,
    /// The file image. Borrowed from the input, never copied.
    pub data: &'a [u8],
    pub writable: bool,
    pub executable: bool,
}

impl Segment<'_> {
    /// Bytes the loader must zero after copying `data`.
    ///
    /// This is `.bss`. A loader that maps `mem_size` but only zeroes what the
    /// file supplied leaves the rest holding whatever the frame previously
    /// contained, which is an information leak across the ring boundary.
    ///
    /// Saturates rather than underflowing. `parse` rejects `filesz > memsz`, so
    /// a `Segment` from [`Elf64::segments`] cannot reach that case; the fields
    /// are `pub`, and a caller-built one with `data.len() > mem_size` returns
    /// `0` here instead of wrapping to ~16 EiB in a build without overflow
    /// checks. It does not report that the `Segment` was malformed, so validate
    /// before constructing one by hand.
    pub fn zero_fill(&self) -> u64 {
        self.mem_size.saturating_sub(self.data.len() as u64)
    }
}

/// A parsed ELF64 executable.
///
/// `Debug` and `PartialEq` are derived so tests can compare a whole
/// `Result<Elf64, ElfError>` in one assertion; comparing only the error side
/// would let a file that should have been refused pass by returning `Ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elf64<'a> {
    bytes: &'a [u8],
    entry: u64,
    phoff: usize,
    phentsize: usize,
    phnum: usize,
}

impl<'a> Elf64<'a> {
    /// Validates the header and program header table.
    ///
    /// Everything the segment iterator later relies on is checked here, so
    /// iteration cannot fail and callers do not have to handle an error per
    /// segment.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ElfError> {
        if bytes.len() < ehdr::SIZE {
            return Err(ElfError::TooShort);
        }
        if bytes[..4] != ELF_MAGIC {
            return Err(ElfError::BadMagic);
        }
        // Class, endianness and version are checked together: all three say
        // "this is not the kind of ELF we parse", and splitting them would
        // suggest the parser could cope with one of them differing.
        if bytes[4] != ELFCLASS64 || bytes[5] != ELFDATA2LSB || bytes[6] != EV_CURRENT {
            return Err(ElfError::NotElf64);
        }

        if read_u16(bytes, ehdr::MACHINE) != EM_X86_64 {
            return Err(ElfError::NotX86_64);
        }
        // Refused rather than best-effort loaded: a PIE binary parses cleanly
        // and then runs at the wrong addresses, which presents as a fault in
        // the program rather than as a loader error.
        if read_u16(bytes, ehdr::TYPE) != ET_EXEC {
            return Err(ElfError::NotExecutable);
        }

        let entry = read_u64(bytes, ehdr::ENTRY);
        let phoff = read_u64(bytes, ehdr::PHOFF);
        let phentsize = read_u16(bytes, ehdr::PHENTSIZE) as usize;
        let phnum = read_u16(bytes, ehdr::PHNUM) as usize;

        // A header smaller than the fields this parser reads would make every
        // program header overlap its neighbour. Larger is allowed: the spec
        // permits an implementation to extend it, and the extra is ignored.
        if phentsize < phdr::SIZE {
            return Err(ElfError::BadProgramHeader);
        }

        // The whole table must lie inside the file. Checked once here with
        // overflow-safe arithmetic, so the per-segment code below can index
        // without re-checking.
        let phoff: usize = phoff.try_into().map_err(|_| ElfError::BadProgramHeader)?;
        let table_len =
            phnum.checked_mul(phentsize).ok_or(ElfError::BadProgramHeader)?;
        let table_end = phoff.checked_add(table_len).ok_or(ElfError::BadProgramHeader)?;
        if table_end > bytes.len() {
            return Err(ElfError::BadProgramHeader);
        }

        // Every segment is validated now rather than lazily, so `segments()`
        // can return a plain iterator. A parse that succeeded and then yielded
        // an error halfway through mapping would leave a half-built address
        // space the caller has no way to reason about.
        let elf = Self { bytes, entry, phoff, phentsize, phnum };
        for index in 0..phnum {
            elf.validate_segment(index)?;
        }
        Ok(elf)
    }

    /// Virtual address of the first instruction.
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// The `PT_LOAD` segments, in the order the file lists them.
    ///
    /// Non-loadable headers are skipped, which is why this is not simply
    /// indexed by program-header number.
    pub fn segments(&self) -> impl Iterator<Item = Segment<'a>> + '_ {
        (0..self.phnum).filter_map(move |index| self.segment(index))
    }

    fn header(&self, index: usize) -> &'a [u8] {
        let start = self.phoff + index * self.phentsize;
        &self.bytes[start..start + phdr::SIZE]
    }

    /// Checks one program header without building a `Segment`.
    ///
    /// Separate from [`Self::segment`] so `parse` can reject a bad file before
    /// any caller sees a partially valid one.
    fn validate_segment(&self, index: usize) -> Result<(), ElfError> {
        let h = self.header(index);
        if read_u32(h, phdr::TYPE) != PT_LOAD {
            return Ok(());
        }
        let offset = read_u64(h, phdr::OFFSET);
        let filesz = read_u64(h, phdr::FILESZ);
        let memsz = read_u64(h, phdr::MEMSZ);

        // `filesz > memsz` means the file image does not fit the memory the
        // segment reserves. Loading it would write past the mapping.
        if filesz > memsz {
            return Err(ElfError::SegmentTooLarge);
        }

        let offset: usize = offset.try_into().map_err(|_| ElfError::BadProgramHeader)?;
        let filesz: usize = filesz.try_into().map_err(|_| ElfError::BadProgramHeader)?;
        let end = offset.checked_add(filesz).ok_or(ElfError::BadProgramHeader)?;
        if end > self.bytes.len() {
            return Err(ElfError::BadProgramHeader);
        }

        // A segment whose virtual range wraps cannot be mapped, and the wrap
        // would turn later bounds checks in the loader into comparisons that
        // silently pass.
        read_u64(h, phdr::VADDR).checked_add(memsz).ok_or(ElfError::BadProgramHeader)?;
        Ok(())
    }

    fn segment(&self, index: usize) -> Option<Segment<'a>> {
        let h = self.header(index);
        if read_u32(h, phdr::TYPE) != PT_LOAD {
            return None;
        }
        let flags = read_u32(h, phdr::FLAGS);
        let offset = read_u64(h, phdr::OFFSET) as usize;
        let filesz = read_u64(h, phdr::FILESZ) as usize;
        Some(Segment {
            vaddr: read_u64(h, phdr::VADDR),
            mem_size: read_u64(h, phdr::MEMSZ),
            // Bounds were established by `validate_segment` during `parse`.
            data: &self.bytes[offset..offset + filesz],
            writable: flags & PF_W != 0,
            executable: flags & PF_X != 0,
        })
    }
}

// Little-endian reads at a known-in-bounds offset. `expect` rather than a
// fallible return: every call site has already established the bound, and
// threading a `Result` through would suggest otherwise.
fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().expect("bounds checked by caller"))
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("bounds checked by caller"))
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("bounds checked by caller"))
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec;
    use std::vec::Vec;

    /// Builds a minimal but valid ELF64 executable with one PT_LOAD segment.
    ///
    /// Hand-built rather than a committed fixture so a test can corrupt one
    /// field at a time and know that field is the only difference.
    fn build(entry: u64, segments: &[(u64, u32, &[u8], u64)]) -> Vec<u8> {
        let phoff = ehdr::SIZE;
        let table = segments.len() * phdr::SIZE;
        let mut data_off = phoff + table;
        let mut out = vec![0u8; data_off];

        out[..4].copy_from_slice(&ELF_MAGIC);
        out[4] = ELFCLASS64;
        out[5] = ELFDATA2LSB;
        out[6] = EV_CURRENT;
        out[ehdr::TYPE..ehdr::TYPE + 2].copy_from_slice(&ET_EXEC.to_le_bytes());
        out[ehdr::MACHINE..ehdr::MACHINE + 2].copy_from_slice(&EM_X86_64.to_le_bytes());
        out[ehdr::ENTRY..ehdr::ENTRY + 8].copy_from_slice(&entry.to_le_bytes());
        out[ehdr::PHOFF..ehdr::PHOFF + 8].copy_from_slice(&(phoff as u64).to_le_bytes());
        out[ehdr::PHENTSIZE..ehdr::PHENTSIZE + 2]
            .copy_from_slice(&(phdr::SIZE as u16).to_le_bytes());
        out[ehdr::PHNUM..ehdr::PHNUM + 2].copy_from_slice(&(segments.len() as u16).to_le_bytes());

        for (i, (vaddr, flags, bytes, memsz)) in segments.iter().enumerate() {
            let h = phoff + i * phdr::SIZE;
            out[h + phdr::TYPE..h + phdr::TYPE + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
            out[h + phdr::FLAGS..h + phdr::FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
            out[h + phdr::OFFSET..h + phdr::OFFSET + 8]
                .copy_from_slice(&(data_off as u64).to_le_bytes());
            out[h + phdr::VADDR..h + phdr::VADDR + 8].copy_from_slice(&vaddr.to_le_bytes());
            out[h + phdr::FILESZ..h + phdr::FILESZ + 8]
                .copy_from_slice(&(bytes.len() as u64).to_le_bytes());
            out[h + phdr::MEMSZ..h + phdr::MEMSZ + 8].copy_from_slice(&memsz.to_le_bytes());
            out.extend_from_slice(bytes);
            data_off += bytes.len();
        }
        out
    }

    fn minimal() -> Vec<u8> {
        build(0x40_0000, &[(0x40_0000, PF_X, &[0x90, 0x90, 0x90, 0x90], 4)])
    }

    #[test]
    fn a_valid_executable_parses_and_reports_its_entry() {
        let bytes = minimal();
        let elf = Elf64::parse(&bytes).expect("valid ELF rejected");
        assert_eq!(elf.entry(), 0x40_0000);
        let segs: Vec<_> = elf.segments().collect();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].vaddr, 0x40_0000);
        assert_eq!(segs[0].data, &[0x90, 0x90, 0x90, 0x90]);
        assert!(segs[0].executable);
        assert!(!segs[0].writable);
    }

    #[test]
    fn permission_flags_map_to_the_right_fields() {
        // A swap here maps a data segment executable, which is the difference
        // between W^X and an executable heap.
        let bytes = build(0x1000, &[(0x1000, PF_W, &[1, 2, 3, 4], 4)]);
        let elf = Elf64::parse(&bytes).unwrap();
        let seg = elf.segments().next().unwrap();
        assert!(seg.writable, "PF_W did not produce a writable segment");
        assert!(!seg.executable, "a non-PF_X segment was reported executable");
    }

    #[test]
    fn a_truncated_file_is_refused() {
        assert_eq!(Elf64::parse(&[]), Err(ElfError::TooShort));
        assert_eq!(Elf64::parse(&[0u8; 8]), Err(ElfError::TooShort));
        assert_eq!(Elf64::parse(&[0u8; ehdr::SIZE - 1]), Err(ElfError::TooShort));
    }

    #[test]
    fn a_file_without_elf_magic_is_refused() {
        let mut bytes = minimal();
        bytes[1] = b'X';
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::BadMagic));
    }

    #[test]
    fn a_32_bit_or_big_endian_file_is_refused() {
        // Both parse far enough to produce plausible garbage if unchecked: a
        // 32-bit header puts e_entry where this parser reads e_phoff.
        let mut bytes = minimal();
        bytes[4] = 1; // ELFCLASS32
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::NotElf64));

        let mut bytes = minimal();
        bytes[5] = 2; // ELFDATA2MSB
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::NotElf64));
    }

    #[test]
    fn a_binary_for_another_architecture_is_refused() {
        let mut bytes = minimal();
        bytes[ehdr::MACHINE..ehdr::MACHINE + 2].copy_from_slice(&183u16.to_le_bytes());
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::NotX86_64));
    }

    #[test]
    fn a_shared_object_is_refused_rather_than_loaded_at_the_wrong_address() {
        // ET_DYN parses cleanly and then runs at addresses nothing relocated.
        // Refusing is what turns that into a loader error instead of a fault
        // inside the program.
        let mut bytes = minimal();
        bytes[ehdr::TYPE..ehdr::TYPE + 2].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::NotExecutable));
    }

    #[test]
    fn a_program_header_table_outside_the_file_is_refused() {
        let mut bytes = minimal();
        bytes[ehdr::PHOFF..ehdr::PHOFF + 8].copy_from_slice(&0xffff_0000u64.to_le_bytes());
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::BadProgramHeader));
    }

    #[test]
    fn a_program_header_count_that_overflows_is_refused() {
        // A header count that puts the table past the end of the file. The
        // `checked_mul` in `parse` cannot actually wrap here, since both
        // operands come from `read_u16` and their product fits comfortably in
        // a `usize` on any 64-bit target; it is belt-and-braces for a narrower
        // host. What this exercises is the `table_end > bytes.len()` bound.
        let mut bytes = minimal();
        bytes[ehdr::PHNUM..ehdr::PHNUM + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::BadProgramHeader));
    }

    #[test]
    fn an_undersized_phentsize_is_refused() {
        // Smaller than the fields this parser reads would make each header
        // overlap the next, so every segment after the first is garbage.
        let mut bytes = minimal();
        bytes[ehdr::PHENTSIZE..ehdr::PHENTSIZE + 2].copy_from_slice(&32u16.to_le_bytes());
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::BadProgramHeader));
    }

    #[test]
    fn a_segment_whose_file_image_runs_past_the_end_is_refused() {
        let mut bytes = minimal();
        let h = ehdr::SIZE;
        bytes[h + phdr::FILESZ..h + phdr::FILESZ + 8]
            .copy_from_slice(&0xffff_0000u64.to_le_bytes());
        bytes[h + phdr::MEMSZ..h + phdr::MEMSZ + 8]
            .copy_from_slice(&0xffff_0000u64.to_le_bytes());
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::BadProgramHeader));
    }

    #[test]
    fn a_segment_with_filesz_above_memsz_is_refused() {
        // The file image would not fit the memory the segment reserves, so
        // copying it writes past the mapping.
        let mut bytes = build(0x1000, &[(0x1000, PF_X, &[1, 2, 3, 4], 4)]);
        let h = ehdr::SIZE;
        bytes[h + phdr::MEMSZ..h + phdr::MEMSZ + 8].copy_from_slice(&2u64.to_le_bytes());
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::SegmentTooLarge));
    }

    #[test]
    fn a_segment_whose_virtual_range_wraps_is_refused() {
        let mut bytes = build(0x1000, &[(0x1000, PF_X, &[1, 2, 3, 4], 4)]);
        let h = ehdr::SIZE;
        bytes[h + phdr::VADDR..h + phdr::VADDR + 8]
            .copy_from_slice(&(u64::MAX - 1).to_le_bytes());
        bytes[h + phdr::MEMSZ..h + phdr::MEMSZ + 8].copy_from_slice(&16u64.to_le_bytes());
        assert_eq!(Elf64::parse(&bytes), Err(ElfError::BadProgramHeader));
    }

    #[test]
    fn non_loadable_headers_are_skipped_not_mapped() {
        let mut bytes = build(0x1000, &[(0x1000, PF_X, &[1, 2, 3, 4], 4)]);
        let h = ehdr::SIZE;
        // PT_NOTE. Mapping it would place file metadata in the address space.
        bytes[h + phdr::TYPE..h + phdr::TYPE + 4].copy_from_slice(&4u32.to_le_bytes());
        let elf = Elf64::parse(&bytes).unwrap();
        assert_eq!(elf.segments().count(), 0, "a non-PT_LOAD header was mapped");
    }

    #[test]
    fn zero_fill_saturates_rather_than_underflowing() {
        // Not reachable through `parse`, which refuses `filesz > memsz`. The
        // fields are public, so the guard is about a caller-built `Segment`.
        let seg = Segment { vaddr: 0, mem_size: 0, data: &[0u8; 8], writable: false, executable: false };
        assert_eq!(seg.zero_fill(), 0, "zero_fill underflowed to a huge range");
    }

    #[test]
    fn zero_fill_reports_the_bss_the_loader_must_clear() {
        // memsz beyond filesz is .bss. A loader that skips it hands the
        // process whatever the frame previously held.
        let bytes = build(0x1000, &[(0x1000, PF_W, &[1, 2, 3, 4], 4096)]);
        let seg = Elf64::parse(&bytes).unwrap().segments().next().unwrap();
        assert_eq!(seg.data.len(), 4);
        assert_eq!(seg.mem_size, 4096);
        assert_eq!(seg.zero_fill(), 4092);
    }

    #[test]
    fn several_segments_are_returned_in_file_order() {
        let bytes = build(
            0x1000,
            &[(0x1000, PF_X, &[1, 2], 2), (0x2000, PF_W, &[3, 4, 5], 3)],
        );
        let segs: Vec<_> = Elf64::parse(&bytes).unwrap().segments().collect();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].vaddr, 0x1000);
        assert_eq!(segs[1].vaddr, 0x2000);
        assert_eq!(segs[1].data, &[3, 4, 5]);
    }
}
