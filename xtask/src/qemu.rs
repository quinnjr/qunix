use anyhow::{Result, bail};
use std::path::Path;
use std::process::Command;

const OVMF_CANDIDATES: &[&str] = &[
    "/usr/share/edk2/x64/OVMF.4m.fd",
    "/usr/share/edk2/x64/OVMF.fd",
    "/usr/share/edk2-ovmf/x64/OVMF.fd",
    "/usr/share/ovmf/x64/OVMF.fd",
    "/usr/share/OVMF/OVMF_CODE.fd",
];

fn find_ovmf() -> Option<String> {
    if let Ok(path) = std::env::var("QUNIX_OVMF") {
        if Path::new(&path).exists() {
            return Some(path);
        }
    }
    OVMF_CANDIDATES
        .iter()
        .find(|p| Path::new(p).exists())
        .map(|p| (*p).to_string())
}

/// Boots the ESP directory under QEMU via OVMF. Returns the process exit code.
pub fn run_esp(esp: &Path, headless: bool) -> Result<i32> {
    let Some(ovmf) = find_ovmf() else {
        bail!("no OVMF firmware found; install edk2-ovmf or set QUNIX_OVMF");
    };

    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.args(["-M", "q35", "-m", "512M"]);
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
        cmd.args(["-display", "none"]);
    }

    let status = cmd.status()?;
    Ok(status.code().unwrap_or(-1))
}
