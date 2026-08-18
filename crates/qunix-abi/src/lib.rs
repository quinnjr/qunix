//! The kernel<->host contract, shared verbatim by both sides.
//!
//! `no_std` and dependency-free so it builds unchanged for the bare-metal
//! kernel target and for the host `xtask` binary. Keeping one definition is
//! what makes the coupling real: a test that compares two copies of the same
//! numbers passes even when both copies drift away from what QEMU does.
#![no_std]

/// Value the kernel writes to QEMU's isa-debug-exit port to report a verdict.
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum ExitCode {
    Success = 0x10,
    Failure = 0x11,
}

/// Host-visible process exit status for a passing run.
pub const HOST_STATUS_SUCCESS: i32 = 33;
/// Host-visible process exit status for a failing run.
pub const HOST_STATUS_FAILURE: i32 = 35;

/// QEMU's isa-debug-exit device exits the process with `(value << 1) | 1`, so
/// the port values and the statuses xtask matches on are one fact, not two.
const _: () = {
    assert!((ExitCode::Success as i32) << 1 | 1 == HOST_STATUS_SUCCESS);
    assert!((ExitCode::Failure as i32) << 1 | 1 == HOST_STATUS_FAILURE);
};

/// Native qunix syscall numbers.
///
/// Deliberately small and deliberately *not* Linux's numbering: the Linux
/// personality is M3's job and will translate. What matches Linux here is the
/// register convention, not the numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Sys {
    Exit = 0,
    Write = 1,
    Yield = 2,
    GetPid = 3,
    Open = 4,
    Read = 5,
    Close = 6,
    Lseek = 7,
    Readdir = 8,
    Stat = 9,
}

impl Sys {
    /// Parses a raw syscall number.
    ///
    /// `None` for anything unrecognised — a userspace process is free to pass
    /// nonsense, and turning that into a `Sys` value by transmute would let it
    /// index a dispatch table out of range.
    pub const fn from_raw(nr: u64) -> Option<Self> {
        match nr {
            0 => Some(Self::Exit),
            1 => Some(Self::Write),
            2 => Some(Self::Yield),
            3 => Some(Self::GetPid),
            4 => Some(Self::Open),
            5 => Some(Self::Read),
            6 => Some(Self::Close),
            7 => Some(Self::Lseek),
            8 => Some(Self::Readdir),
            9 => Some(Self::Stat),
            _ => None,
        }
    }
}

/// Error returns. Negative so a syscall can return a non-negative result or an
/// error in one register, as Linux does.
///
/// The values are qunix's own, not Linux's: `qunix-abi` is the *native* ABI,
/// and the Linux personality translates. Distinctness is what matters, and is
/// asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum Errno {
    Ok = 0,
    BadSyscall = -1,
    BadAddress = -2,
    BadArgument = -3,
    /// No such file or directory.
    NoEntry = -4,
    /// A path component that had to be a directory was not one.
    NotDirectory = -5,
    /// A directory was named where a file was required.
    IsDirectory = -6,
    /// The descriptor is not open in this process.
    BadDescriptor = -7,
    /// The path, or one component of it, is longer than the kernel accepts.
    NameTooLong = -8,
    /// Symlink resolution did not terminate within the bound.
    TooManyLinks = -9,
    /// The process's descriptor table is full.
    TooManyFiles = -10,
    /// The kernel does not implement this operation — either a filesystem that
    /// genuinely lacks it, or a syscall number that is reserved but not yet
    /// wired into the dispatcher.
    NotSupported = -11,
}

/// How a file was opened.
///
/// A newtype over the bits rather than an enum, because the access mode and the
/// future creation flags are independent. `from_bits` refuses anything
/// undefined: the word is userspace input, and accepting an unknown bit means a
/// flag defined later silently inherits behaviour from programs written before
/// it existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFlags(u32);

impl OpenFlags {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    /// Every bit this kernel defines. The mask is derived from the constants
    /// rather than written out, so adding one cannot leave the guard behind.
    const KNOWN: u32 = Self::READ.0 | Self::WRITE.0;

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !Self::KNOWN != 0 { None } else { Some(Self(bits)) }
    }

    pub const fn is_readable(self) -> bool {
        self.0 & Self::READ.0 != 0
    }

    pub const fn is_writable(self) -> bool {
        self.0 & Self::WRITE.0 != 0
    }
}

/// The reference point for [`Sys::Lseek`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Whence {
    Set = 0,
    Current = 1,
    End = 2,
}

impl Whence {
    pub const fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            0 => Some(Self::Set),
            1 => Some(Self::Current),
            2 => Some(Self::End),
            _ => None,
        }
    }
}

/// What `stat` writes into the user's buffer.
///
/// `repr(C)` because userspace reads it field by field. The padding is explicit
/// so the layout is the same on both sides of the call rather than the same by
/// luck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Stat {
    pub size: u64,
    pub inode: u64,
    pub kind: u32,
    pub mode: u32,
    pub links: u64,
}

/// What `readdir` writes for one entry.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct DirEntry {
    pub inode: u64,
    pub kind: u32,
    pub name_len: u32,
    pub name: [u8; NAME_MAX],
}

/// The longest single path component, in bytes.
pub const NAME_MAX: usize = 248;
/// The longest whole path, in bytes.
pub const PATH_MAX: usize = 4096;

/// What a [`Stat`] or [`DirEntry`] names.
pub const KIND_FILE: u32 = 1;
pub const KIND_DIRECTORY: u32 = 2;

#[cfg(test)]
mod syscall_tests {
    use super::*;

    #[test]
    fn every_declared_syscall_round_trips() {
        for sys in [
            Sys::Exit, Sys::Write, Sys::Yield, Sys::GetPid,
            Sys::Open, Sys::Read, Sys::Close, Sys::Lseek, Sys::Readdir, Sys::Stat,
        ] {
            assert_eq!(Sys::from_raw(sys as u64), Some(sys));
        }
    }

    #[test]
    fn an_unknown_number_is_rejected_rather_than_transmuted() {
        // The negative direction: userspace passes whatever it likes, and a
        // `Sys` conjured from an out-of-range number would index a dispatch
        // table past its end.
        assert_eq!(Sys::from_raw(10), None);
        assert_eq!(Sys::from_raw(u64::MAX), None);
    }

    #[test]
    fn a_syscall_number_is_never_reused() {
        // Two names for one number is a userspace program calling one thing and
        // reaching another, which no test of either alone would show.
        //
        // No `alloc` in this crate, so the dedup check sorts a fixed-size array
        // in place and compares adjacent elements, rather than using `Vec`.
        let numbers = [
            Sys::Exit as u64, Sys::Write as u64, Sys::Yield as u64, Sys::GetPid as u64,
            Sys::Open as u64, Sys::Read as u64, Sys::Close as u64, Sys::Lseek as u64,
            Sys::Readdir as u64, Sys::Stat as u64,
        ];
        let mut sorted = numbers;
        sorted.sort_unstable();
        for pair in sorted.windows(2) {
            assert_ne!(pair[0], pair[1], "two syscalls share a number: {numbers:?}");
        }
    }

    #[test]
    fn errors_are_negative_so_they_cannot_be_confused_with_a_result() {
        assert!((Errno::BadSyscall as i64) < 0);
        assert!((Errno::BadAddress as i64) < 0);
        assert!((Errno::BadArgument as i64) < 0);
        assert_eq!(Errno::Ok as i64, 0);
    }

    #[test]
    fn every_errno_is_negative_and_distinct() {
        // Negative so a syscall returns a result or an error in one register.
        // Distinct because a caller that cannot tell `NoEntry` from `NotDirectory`
        // reports the wrong thing to the program.
        //
        // No `alloc` in this crate, so distinctness is checked by sorting a
        // fixed-size array in place and comparing adjacent elements, rather than
        // collecting into a `Vec` and calling `dedup`.
        let all = [
            Errno::BadSyscall, Errno::BadAddress, Errno::BadArgument, Errno::NoEntry,
            Errno::NotDirectory, Errno::IsDirectory, Errno::BadDescriptor,
            Errno::NameTooLong, Errno::TooManyLinks, Errno::TooManyFiles,
            Errno::NotSupported,
        ];
        for e in all {
            assert!((e as i64) < 0, "{e:?} is not negative");
        }
        let mut codes: [i64; 11] = all.map(|e| e as i64);
        codes.sort_unstable();
        for pair in codes.windows(2) {
            assert_ne!(pair[0], pair[1], "two errnos share a code");
        }
    }

    #[test]
    fn open_flags_are_disjoint_bits_and_reject_the_unknown() {
        // A flag word is userspace input. Accepting bits nobody defined means a
        // later flag silently inherits behaviour from a program written before it.
        assert_eq!(OpenFlags::READ.bits() & OpenFlags::WRITE.bits(), 0);
        assert!(OpenFlags::from_bits(OpenFlags::READ.bits()).is_some());
        assert!(
            OpenFlags::from_bits(0x8000_0000).is_none(),
            "an undefined flag bit was accepted"
        );
        // Neither direction requested is not a readable or writable file.
        assert!(!OpenFlags::from_bits(0).unwrap().is_readable());
    }

    #[test]
    fn a_whence_is_parsed_rather_than_transmuted() {
        assert_eq!(Whence::from_raw(0), Some(Whence::Set));
        assert_eq!(Whence::from_raw(1), Some(Whence::Current));
        assert_eq!(Whence::from_raw(2), Some(Whence::End));
        assert_eq!(Whence::from_raw(3), None, "an unknown whence was accepted");
    }

    #[test]
    fn stat_and_direntry_have_the_layout_userspace_reads() {
        // The kernel writes these into a user buffer, so their size and field
        // order are a contract. A field added in the middle silently shifts every
        // field after it in a program compiled against the old layout.
        assert_eq!(core::mem::size_of::<Stat>(), 32);
        assert_eq!(core::mem::align_of::<Stat>(), 8);
        assert_eq!(core::mem::size_of::<DirEntry>(), 264);
    }
}
