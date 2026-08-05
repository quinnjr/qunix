use anyhow::{Context, Result, bail};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Only unified OVMF images are listed. Split builds (`OVMF_CODE.fd` plus a
/// companion writable `OVMF_VARS.fd`) need a second pflash unit for their NVRAM
/// store; loaded alone and read-only they fail in distro-dependent ways. A user
/// with only a split build gets the "set QUNIX_OVMF" error instead, which points
/// at a fix rather than at a confusing firmware hang.
const OVMF_CANDIDATES: &[&str] = &[
    "/usr/share/edk2/x64/OVMF.4m.fd",
    "/usr/share/edk2/x64/OVMF.fd",
    "/usr/share/edk2-ovmf/x64/OVMF.fd",
    "/usr/share/ovmf/x64/OVMF.fd",
    // Debian and Ubuntu's `ovmf` package. Their OVMF_CODE_4M.fd is a split
    // build and deliberately absent from this list.
    "/usr/share/ovmf/OVMF.fd",
];

/// Wall-clock budget for a single QEMU run, overridable with `QUNIX_QEMU_TIMEOUT`.
/// Without it a kernel deadlock (or a harness that never fires isa-debug-exit)
/// hangs `cargo xtask test` forever; `-no-reboot` only covers triple faults.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// How often to check whether QEMU exited. This is pure detection latency on
/// every run, so keep it small; the syscall cost is negligible either way.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How QEMU terminated. Signal deaths are kept distinct from exit codes because
/// the runner's contract is defined in terms of isa-debug-exit status values,
/// and collapsing both into one integer makes a crash look like a test result.
pub enum Exit {
    Code(i32),
    Signal(i32),
}

fn timeout() -> Result<Duration> {
    let secs = match std::env::var("QUNIX_QEMU_TIMEOUT") {
        Ok(raw) => raw.trim().parse::<u64>().with_context(|| {
            format!("QUNIX_QEMU_TIMEOUT is not a whole number of seconds: {raw:?}")
        })?,
        Err(_) => DEFAULT_TIMEOUT_SECS,
    };
    // A zero budget expires on the first poll, killing QEMU before OVMF has
    // even started; nobody means that by "0", so reject it rather than let it
    // read as "unlimited".
    if secs == 0 {
        bail!("QUNIX_QEMU_TIMEOUT=0 would kill qemu immediately; 0 does not mean unlimited");
    }
    Ok(Duration::from_secs(secs))
}

/// Whether KVM can actually be opened, not merely whether the node exists.
fn kvm_usable() -> bool {
    std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok()
}

fn find_ovmf() -> Option<String> {
    if let Ok(path) = std::env::var("QUNIX_OVMF")
        && Path::new(&path).exists()
    {
        return Some(path);
    }
    OVMF_CANDIDATES
        .iter()
        .find(|p| Path::new(p).exists())
        .map(|p| (*p).to_string())
}

/// Boots the ESP directory under QEMU via OVMF, killing it if it outlives the
/// timeout. Returns how the process terminated.
pub fn run_esp(esp: &Path, headless: bool) -> Result<Exit> {
    let Some(ovmf) = find_ovmf() else {
        bail!("no OVMF firmware found; install edk2-ovmf or set QUNIX_OVMF");
    };

    let mut cmd = Command::new("qemu-system-x86_64");
    // The `accel=kvm:tcg` fallback list is a `-machine` property; the standalone
    // `-accel` flag takes exactly one accelerator and rejects the list form.
    // KVM is used when /dev/kvm is usable and emulation otherwise, so this is
    // safe in CI containers. Under TCG every guest instruction is translated,
    // and the bulk of a test run is OVMF firmware init.
    cmd.args(["-M", "q35,accel=kvm:tcg", "-m", "512M"]);
    // Four CPUs so SMP bring-up is actually exercised. A single-CPU guest makes
    // every AP test vacuous -- it would assert that zero processors came
    // online, which is true of a kernel that cannot start any.
    cmd.args(["-smp", "4"]);
    // Existence is not usability: /dev/kvm is typically 0660 root:kvm, so a
    // user outside that group (or a container without the device cgroup) gets
    // the silent kvm->tcg fallback. `-cpu host` is rejected outright under TCG
    // ("CPU model 'host' requires KVM or HVF"), which would fail every run.
    // `max` is valid under both and gives TCG a modern feature set.
    if kvm_usable() {
        cmd.args(["-cpu", "host"]);
    } else {
        cmd.args(["-cpu", "max"]);
    }
    // Suppresses the default NIC, display, and legacy devices that OVMF would
    // otherwise enumerate before reaching the ESP. Everything the harness needs
    // (serial, isa-debug-exit) is added explicitly below. `-net none` is not
    // also needed: -nodefaults already covers the default NIC.
    cmd.arg("-nodefaults");
    cmd.arg("-drive");
    cmd.arg(format!("if=pflash,format=raw,readonly=on,file={ovmf}"));
    // VVFAT presents the directory as a FAT filesystem, so no image tooling
    // (xorriso, mtools, loop mounts) is needed to produce a bootable volume.
    cmd.arg("-drive");
    cmd.arg(format!("format=raw,file=fat:rw:{}", esp.display()));
    // `-no-shutdown` is deliberately absent: it would keep QEMU alive after
    // isa-debug-exit fires, so the harness could never observe an exit code.
    cmd.args(["-serial", "stdio", "-no-reboot"]);
    cmd.args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]);
    if headless {
        // No display device at all: OVMF skips video-driver init and nothing
        // consumes a framebuffer.
        cmd.args(["-display", "none"]);
    } else {
        // `-nodefaults` removed the default VGA adapter, so the interactive
        // path has to ask for one back or it gets no output window.
        cmd.args(["-vga", "std"]);
    }

    let budget = timeout()?;
    // `Instant + Duration` panics on overflow, so an absurd but well-formed
    // QUNIX_QEMU_TIMEOUT would abort past the error handling the parse wrote.
    // Computed before the spawn so a rejected budget leaves no orphan QEMU.
    let deadline = Instant::now().checked_add(budget).context("QUNIX_QEMU_TIMEOUT is too large")?;
    let mut child = cmd.spawn().context("failed to launch qemu-system-x86_64")?;

    let status = loop {
        if let Some(status) = child.try_wait().context("waiting on qemu")? {
            break status;
        }
        if Instant::now() >= deadline {
            // The child owns the terminal's stdio, so leaving it running would
            // poison every later invocation; reap it before reporting.
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "qemu did not exit within {}s (set QUNIX_QEMU_TIMEOUT to change); \
                 it was killed. The kernel most likely hung before reaching \
                 isa-debug-exit",
                budget.as_secs()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    match (status.code(), status.signal()) {
        (Some(code), _) => Ok(Exit::Code(code)),
        (None, Some(signo)) => Ok(Exit::Signal(signo)),
        // Unreachable on Unix: a reaped child either exited or was signalled.
        (None, None) => bail!("qemu terminated without an exit code or signal"),
    }
}
