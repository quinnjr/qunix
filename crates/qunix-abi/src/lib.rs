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
            _ => None,
        }
    }
}

/// Error returns. Negative so a syscall can return a non-negative result or an
/// error in one register, as Linux does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum Errno {
    Ok = 0,
    BadSyscall = -1,
    BadAddress = -2,
    BadArgument = -3,
}

#[cfg(test)]
mod syscall_tests {
    use super::*;

    #[test]
    fn every_declared_syscall_round_trips() {
        for sys in [Sys::Exit, Sys::Write, Sys::Yield, Sys::GetPid] {
            assert_eq!(Sys::from_raw(sys as u64), Some(sys));
        }
    }

    #[test]
    fn an_unknown_number_is_rejected_rather_than_transmuted() {
        // The negative direction: userspace passes whatever it likes, and a
        // `Sys` conjured from an out-of-range number would index a dispatch
        // table past its end.
        assert_eq!(Sys::from_raw(4), None);
        assert_eq!(Sys::from_raw(u64::MAX), None);
    }

    #[test]
    fn errors_are_negative_so_they_cannot_be_confused_with_a_result() {
        assert!((Errno::BadSyscall as i64) < 0);
        assert!((Errno::BadAddress as i64) < 0);
        assert!((Errno::BadArgument as i64) < 0);
        assert_eq!(Errno::Ok as i64, 0);
    }
}
