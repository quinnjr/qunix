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
    parse_timeout(std::env::var("QUNIX_QEMU_TIMEOUT").ok().as_deref())
}

/// The wall-clock budget a `QUNIX_QEMU_TIMEOUT` value asks for.
///
/// Pure, because every rejection here is a fail-open if it stops rejecting:
/// a value that silently fell back to the default would give a run nobody
/// asked for, and a zero that read as "unlimited" would hang CI on the exact
/// deadlock the budget exists to bound. Reading the variable inline made all
/// three unreachable from a test, since mutating the environment races every
/// other test in the process.
fn parse_timeout(raw: Option<&str>) -> Result<Duration> {
    let secs = match raw {
        Some(raw) => raw.trim().parse::<u64>().with_context(|| {
            format!("QUNIX_QEMU_TIMEOUT is not a whole number of seconds: {raw:?}")
        })?,
        None => DEFAULT_TIMEOUT_SECS,
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

fn find_ovmf() -> Result<String> {
    let override_path = std::env::var("QUNIX_OVMF").ok();
    select_ovmf(override_path.as_deref(), &|p| Path::new(p).exists())
}

/// Picks the firmware image, given a way to ask whether a path exists.
///
/// The existence test is a parameter so the override branch is reachable from
/// a test: it needs `QUNIX_OVMF` pointed at a missing path, and mutating the
/// environment from a test races every other test in the process — which is
/// why the baseline note records this branch as uncoverable. It is the branch
/// most worth covering, because falling back to a distro image when the user
/// named one is how a run silently boots firmware nobody asked for.
fn select_ovmf(override_path: Option<&str>, exists: &dyn Fn(&str) -> bool) -> Result<String> {
    // An explicit override that does not exist is an error, not a reason to
    // fall back. Returning `None` here was not enough: the caller's message is
    // "install edk2-ovmf or set QUNIX_OVMF", which tells someone who just set
    // it to set it, and never names the path that is missing.
    if let Some(path) = override_path {
        if !exists(path) {
            bail!("QUNIX_OVMF is set to `{path}`, which does not exist; unset it to search the usual locations");
        }
        return Ok(path.to_string());
    }
    OVMF_CANDIDATES
        .iter()
        .find(|p| exists(p))
        .map(|p| (*p).to_string())
        .context("no OVMF firmware found; install edk2-ovmf or set QUNIX_OVMF")
}

/// The full QEMU command line, minus the program name.
///
/// Extracted from the spawn because every element of it is a decision, and
/// several of them fail *green* when they are wrong rather than failing at all:
/// drop `isa-debug-exit` and the harness has no way to report a verdict, add
/// `-no-shutdown` and it can never be observed, and reduce `-smp` to 1 and the
/// SMP tests assert things about a machine that has no APs while still passing.
/// See the tests below for what each argument is guarding.
/// Asks a running QEMU where each vCPU is, through the human monitor.
///
/// The failure this exists for is a *hard* lockup: every processor spinning
/// with interrupts masked, so no in-kernel diagnostic can run -- the timer
/// interrupt that would drive one is exactly what is not being delivered. The
/// hypervisor is outside that, and `info registers -a` names the instruction
/// each vCPU is executing, which is the difference between "the kernel hung"
/// and a symbol to look at.
///
/// Best-effort by construction: this runs while reporting a failure, so a
/// monitor that cannot be reached must not replace the real error with its own.
fn dump_cpu_state(socket: &Path) -> Option<String> {
    use std::io::{Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.write_all(b"info registers -a\n").ok()?;
    stream.flush().ok()?;
    // Read until the monitor goes quiet rather than until a prompt: the banner,
    // the echo and the prompt all arrive interleaved with the payload, and a
    // parser for that is more ways to lose the output than it is worth.
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = stream.read(&mut buf) {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() > 256 * 1024 {
            break;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn qemu_args(ovmf: &str, esp: &Path, disk: &Path, headless: bool, kvm: bool) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    // A macro rather than a closure: a closure capturing `args` mutably blocks
    // the `format!` pushes below from borrowing it.
    macro_rules! push {
        ($($item:expr),+ $(,)?) => {{ $(args.push(String::from($item));)+ }};
    }

    // The `accel=kvm:tcg` fallback list is a `-machine` property; the standalone
    // `-accel` flag takes exactly one accelerator and rejects the list form.
    // KVM is used when /dev/kvm is usable and emulation otherwise, so this is
    // safe in CI containers. Under TCG every guest instruction is translated,
    // and the bulk of a test run is OVMF firmware init.
    push!("-M", "q35,accel=kvm:tcg", "-m", "512M");
    // Four CPUs so SMP bring-up is actually exercised. A single-CPU guest makes
    // every AP test vacuous -- it would assert that zero processors came
    // online, which is true of a kernel that cannot start any.
    push!("-smp", "4");
    // Existence is not usability: /dev/kvm is typically 0660 root:kvm, so a
    // user outside that group (or a container without the device cgroup) gets
    // the silent kvm->tcg fallback. `-cpu host` is rejected outright under TCG
    // ("CPU model 'host' requires KVM or HVF"), which would fail every run.
    // `max` is valid under both and gives TCG a modern feature set.
    push!("-cpu", if kvm { "host" } else { "max" });
    // Suppresses the default NIC, display, and legacy devices that OVMF would
    // otherwise enumerate before reaching the ESP. Everything the harness needs
    // (serial, isa-debug-exit) is added explicitly below. `-net none` is not
    // also needed: -nodefaults already covers the default NIC.
    push!("-nodefaults");
    push!("-drive");
    args.push(format!("if=pflash,format=raw,readonly=on,file={ovmf}"));
    // VVFAT presents the directory as a FAT filesystem, so no image tooling
    // (xorriso, mtools, loop mounts) is needed to produce a bootable volume.
    push!("-drive");
    args.push(format!("format=raw,file=fat:rw:{}", esp.display()));
    // `-no-shutdown` is deliberately absent: it would keep QEMU alive after
    // isa-debug-exit fires, so the harness could never observe an exit code.
    // The block device the driver talks to. Two halves of one thing: a drive
    // with no device is invisible to the guest, and a device naming no drive
    // makes QEMU refuse to start -- so they are joined by id.
    //
    // `disable-legacy=on` is not decoration. It makes QEMU refuse to present
    // the legacy interface at all, so a driver bug that falls back to it fails
    // here rather than working under QEMU and breaking on hardware, where the
    // legacy interface may simply be absent.
    push!("-drive");
    args.push(format!("format=raw,if=none,id=qunixdisk,file={}", disk.display()));
    push!("-device");
    args.push(String::from(
        "virtio-blk-pci,drive=qunixdisk,disable-legacy=on,disable-modern=off",
    ));
    push!("-serial", "stdio", "-no-reboot");
    push!("-device", "isa-debug-exit,iobase=0xf4,iosize=0x04");
    if headless {
        // No display device at all: OVMF skips video-driver init and nothing
        // consumes a framebuffer.
        push!("-display", "none");
    } else {
        // `-nodefaults` removed the default VGA adapter, so the interactive
        // path has to ask for one back or it gets no output window.
        push!("-vga", "std");
    }
    args
}

/// Boots the ESP directory under QEMU via OVMF, killing it if it outlives the
/// timeout. Returns how the process terminated.
pub fn run_esp(esp: &Path, disk: &Path, headless: bool) -> Result<Exit> {
    let ovmf = find_ovmf()?;

    // One socket per invocation: `cargo xtask test` boots twice, and a shared
    // path would leave the second run talking to the first run's stale node.
    let monitor = std::env::temp_dir().join(format!("qunix-monitor-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&monitor);

    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.args(qemu_args(&ovmf, esp, disk, headless, kvm_usable()));
    cmd.arg("-monitor");
    cmd.arg(format!("unix:{},server,nowait", monitor.display()));

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
            // Asked *before* the kill: the registers are the only evidence of
            // where a hung kernel stopped, and a dead QEMU has none.
            let state = dump_cpu_state(&monitor);
            // The child owns the terminal's stdio, so leaving it running would
            // poison every later invocation; reap it before reporting.
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&monitor);
            let where_ = match state {
                Some(text) => format!("\n\nvCPU state at the timeout:\n{text}"),
                None => String::from(
                    "\n\n(the qemu monitor could not be reached, so there is no vCPU state)",
                ),
            };
            bail!(
                "qemu did not exit within {}s (set QUNIX_QEMU_TIMEOUT to change); \
                 it was killed. The kernel most likely hung before reaching \
                 isa-debug-exit{where_}",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(headless: bool, kvm: bool) -> Vec<String> {
        qemu_args("/fw/OVMF.fd", Path::new("/t/esp"), Path::new("/t/disk.img"), headless, kvm)
    }

    #[test]
    fn the_test_disk_reaches_the_guest_as_a_virtio_block_device() {
        let a = args(true, false);
        // The drive and the device are two halves of one thing, and either
        // alone looks like success in an argument list: a drive with no device
        // is invisible to the guest, and a device with no drive makes QEMU
        // refuse to start.
        assert!(a.iter().any(|x| x.contains("file=/t/disk.img")), "no drive: {a:?}");
        assert!(
            a.windows(2).any(|w| w[0] == "-device" && w[1].starts_with("virtio-blk-pci")),
            "no device: {a:?}"
        );
        // And they must be joined by id, or the device is attached to nothing.
        let drive = a.iter().find(|x| x.contains("id=qunixdisk")).expect("the drive has no id");
        assert!(drive.contains("if=none"), "the drive is also attached implicitly: {drive}");
        assert!(
            a.iter().any(|x| x.contains("drive=qunixdisk")),
            "the device does not name the drive: {a:?}"
        );
        // Legacy refused, so a driver that falls back to it fails here rather
        // than working under QEMU and breaking on hardware.
        assert!(
            a.iter().any(|x| x.contains("disable-legacy=on")),
            "the legacy interface is still offered: {a:?}"
        );
    }

    /// The value following `flag`, if the flag is present at all.
    fn value_of(args: &[String], flag: &str) -> Option<String> {
        args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
    }

    #[test]
    fn the_guest_has_more_than_one_cpu() {
        // The vacuous-green case, and the reason this is asserted rather than
        // left to review: with `-smp 1` the SMP tests assert that the APs they
        // can see behaved, there are none, and every one of them passes. The
        // run stays green while testing nothing about bring-up, stealing or
        // shootdown. Only the floor is pinned -- raising the count is fine.
        let smp: u32 = value_of(&args(true, false), "-smp")
            .expect("no -smp argument")
            .parse()
            .expect("-smp is not a plain CPU count");
        assert!(smp >= 2, "-smp {smp} makes every AP test vacuous while still passing");
    }

    #[test]
    fn the_harness_can_always_report_a_verdict() {
        // isa-debug-exit is the only channel the in-QEMU harness has. Without
        // the device the kernel's port write goes nowhere and QEMU is killed by
        // the timeout instead -- which at least fails loudly. `-no-shutdown` is
        // the dangerous half: QEMU stays alive after the write, so the verdict
        // is never observed and the run reports a timeout rather than the
        // failure the kernel actually signalled.
        for headless in [true, false] {
            let a = args(headless, false);
            assert!(
                a.windows(2).any(|w| w[0] == "-device" && w[1].starts_with("isa-debug-exit")),
                "no isa-debug-exit device: {a:?}"
            );
            assert!(
                !a.iter().any(|x| x == "-no-shutdown"),
                "-no-shutdown keeps qemu alive past isa-debug-exit, so no exit code is ever seen"
            );
            // A triple fault would otherwise loop the guest forever inside the
            // timeout rather than terminating.
            assert!(a.iter().any(|x| x == "-no-reboot"), "{a:?}");
            // Console output is the only diagnostic a failing kernel produces.
            assert_eq!(value_of(&a, "-serial").as_deref(), Some("stdio"), "{a:?}");
        }
    }

    #[test]
    fn the_esp_and_firmware_reach_the_right_drives() {
        let a = args(true, false);
        // Firmware read-only on pflash; swapping the two drive forms boots the
        // ESP as firmware and produces a hang with no message.
        assert!(
            a.iter().any(|x| x == "if=pflash,format=raw,readonly=on,file=/fw/OVMF.fd"),
            "{a:?}"
        );
        assert!(a.iter().any(|x| x == "format=raw,file=fat:rw:/t/esp"), "{a:?}");
    }

    #[test]
    fn cpu_host_is_only_used_when_kvm_is_usable() {
        // `-cpu host` is rejected outright under TCG, so getting this backwards
        // fails every run on a machine without /dev/kvm -- including CI.
        assert_eq!(value_of(&args(true, true), "-cpu").as_deref(), Some("host"));
        assert_eq!(value_of(&args(true, false), "-cpu").as_deref(), Some("max"));
    }

    #[test]
    fn headless_removes_the_display_and_interactive_restores_a_vga_adapter() {
        // `-nodefaults` dropped the default adapter, so the interactive path
        // asks for one back. Without it `xtask run` opens a window with no
        // output and looks like a kernel that never booted.
        let head = args(false, false);
        assert_eq!(value_of(&head, "-vga").as_deref(), Some("std"), "{head:?}");
        assert!(!head.iter().any(|x| x == "-display"), "{head:?}");

        let less = args(true, false);
        assert_eq!(value_of(&less, "-display").as_deref(), Some("none"), "{less:?}");
        assert!(!less.iter().any(|x| x == "-vga"), "{less:?}");
    }

    #[test]
    fn a_malformed_timeout_is_refused_rather_than_defaulted() {
        // Falling back to 120s for `QUNIX_QEMU_TIMEOUT=5m` gives a run the user
        // believes was five minutes; the error has to name the value so the
        // typo is visible.
        let err = parse_timeout(Some("5m")).unwrap_err();
        assert!(err.to_string().contains("5m"), "unhelpful error: {err}");
        assert!(parse_timeout(Some("-1")).is_err());
        assert!(parse_timeout(Some("")).is_err());
    }

    #[test]
    fn a_zero_timeout_is_refused_rather_than_read_as_unlimited() {
        // Zero expires on the first poll, killing qemu before OVMF has started,
        // which presents as "the kernel hung" on a kernel that never ran.
        let err = parse_timeout(Some("0")).unwrap_err();
        assert!(err.to_string().contains("unlimited"), "unhelpful error: {err}");
        assert!(parse_timeout(Some(" 0 ")).is_err(), "whitespace smuggled a zero budget through");
    }

    #[test]
    fn an_unset_timeout_uses_the_default_budget() {
        assert_eq!(parse_timeout(None).unwrap(), Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(parse_timeout(Some(" 30\n")).unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn an_override_naming_a_missing_file_is_fatal_rather_than_a_fallback() {
        // The branch the baseline records as uncoverable, and the one that
        // matters: silently searching the distro locations boots firmware the
        // user did not name, and the generic "install edk2-ovmf or set
        // QUNIX_OVMF" message tells someone who just set it to set it.
        let err = select_ovmf(Some("/nope/OVMF.fd"), &|_| false).unwrap_err();
        assert!(err.to_string().contains("/nope/OVMF.fd"), "the missing path is not named: {err}");
        // Even with every distro candidate present, the override still wins.
        let err = select_ovmf(Some("/nope/OVMF.fd"), &|p| p != "/nope/OVMF.fd").unwrap_err();
        assert!(err.to_string().contains("/nope/OVMF.fd"), "the override fell back: {err}");
    }

    #[test]
    fn an_override_that_exists_is_used_verbatim() {
        assert_eq!(select_ovmf(Some("/my/OVMF.fd"), &|_| true).unwrap(), "/my/OVMF.fd");
    }

    #[test]
    fn a_split_ovmf_build_is_not_picked_up() {
        // Split builds need a second pflash unit for their NVRAM store and hang
        // in distro-dependent ways when loaded alone. Only the unified images
        // may be auto-selected; a user with only a split build must get the
        // "set QUNIX_OVMF" error, which points at a fix.
        for candidate in OVMF_CANDIDATES {
            assert!(
                !candidate.contains("CODE"),
                "{candidate} is a split build and must not be auto-selected"
            );
        }
        let err = select_ovmf(None, &|p| p.contains("OVMF_CODE")).unwrap_err();
        assert!(err.to_string().contains("QUNIX_OVMF"), "{err}");
    }

    #[test]
    fn the_first_present_candidate_wins() {
        let found = select_ovmf(None, &|p| p == OVMF_CANDIDATES[1]).unwrap();
        assert_eq!(found, OVMF_CANDIDATES[1]);
    }
}
