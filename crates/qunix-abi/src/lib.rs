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
