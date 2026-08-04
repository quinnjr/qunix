use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

const LIMINE_BRANCH: &str = "v11.x-binary";
const LIMINE_REPO: &str = "https://github.com/limine-bootloader/limine.git";

/// Clones Limine's prebuilt binaries once.
///
/// Only `BOOTX64.EFI` is needed: qunix boots via UEFI from a VVFAT-backed ESP,
/// so neither the host `limine` tool nor the BIOS install step is required.
pub fn ensure_limine(root: &Path) -> Result<PathBuf> {
    let dir = root.join("target/limine");
    if !dir.join("BOOTX64.EFI").exists() {
        let status = Command::new("git")
            .args(["clone", "--depth", "1", "--branch", LIMINE_BRANCH, LIMINE_REPO])
            .arg(&dir)
            .status()
            .context("failed to run git clone for limine")?;
        if !status.success() {
            bail!("cloning limine failed");
        }
    }
    Ok(dir)
}

/// Assembles an EFI System Partition as a plain directory.
///
/// QEMU serves this directly with `-drive format=raw,file=fat:rw:<dir>`, so no
/// ISO or filesystem-image tooling is involved.
pub fn build_esp(root: &Path, kernel: &Path) -> Result<PathBuf> {
    let limine = ensure_limine(root)?;
    let esp = root.join("target/esp");
    let _ = std::fs::remove_dir_all(&esp);
    std::fs::create_dir_all(esp.join("EFI/BOOT"))?;
    std::fs::create_dir_all(esp.join("boot/limine"))?;

    std::fs::copy(limine.join("BOOTX64.EFI"), esp.join("EFI/BOOT/BOOTX64.EFI"))
        .context("copying BOOTX64.EFI")?;
    std::fs::copy(root.join("limine.conf"), esp.join("boot/limine/limine.conf"))
        .context("copying limine.conf")?;
    std::fs::copy(kernel, esp.join("boot/qunix-kernel")).context("copying the kernel")?;

    Ok(esp)
}
