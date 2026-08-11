use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

const LIMINE_BRANCH: &str = "v11.x-binary";
const LIMINE_REPO: &str = "https://github.com/limine-bootloader/limine.git";

/// Pinned commit on `v11.x-binary`. That branch is mutable and its tip is a
/// prebuilt binary the guest firmware executes, so tracking it would mean
/// running whatever was pushed most recently, unreviewed.
const LIMINE_COMMIT: &str = "5be26a73d7b7b4d4477d18be94e1d16e615adf56";

/// SHA-256 of `BOOTX64.EFI` at `LIMINE_COMMIT`. Content, not size: a length
/// floor accepts any sufficiently large file, including a substituted one.
const LIMINE_BOOTX64_SHA256: &str =
    "333f7a69379b1f47e019be215885cdc078b9edd6e9255cbe2f706d6c5ec0ef06";

/// Clones Limine's prebuilt binaries once and verifies them on every run.
///
/// Only `BOOTX64.EFI` is needed: qunix boots via UEFI from a VVFAT-backed ESP,
/// so neither the host `limine` tool nor the BIOS install step is required.
pub fn ensure_limine(root: &Path) -> Result<PathBuf> {
    let dir = root.join(".limine");
    let bootx64 = dir.join("BOOTX64.EFI");
    if !bootx64.exists() {
        // An interrupted clone leaves `.limine/` present but without the payload,
        // and `git clone` refuses a non-empty destination. Clearing it first is
        // what makes the retry work instead of wedging every later build.
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| {
                format!("removing incomplete limine checkout at {}", dir.display())
            })?;
        }
        // `--no-checkout` keeps the branch tip out of the working tree entirely,
        // so the only bytes that ever land on disk are the pinned commit's.
        git(
            root,
            &["clone", "--no-checkout", "--depth", "1", "--branch", LIMINE_BRANCH, LIMINE_REPO],
            Some(&dir),
        )?;
        // A shallow clone of the branch need not contain the pin once the branch
        // has moved on, so fetch the commit by id before checking it out.
        git(&dir, &["fetch", "--depth", "1", "origin", LIMINE_COMMIT], None)?;
        git(&dir, &["checkout", "--detach", LIMINE_COMMIT], None)?;
    }

    // Re-verified every run rather than only after a clone: `ensure_limine` is
    // otherwise trust-on-first-use, and a checkout corrupted or swapped after
    // the fact would be handed to the guest forever.
    let digest = sha256_file(&bootx64)?;
    if digest != LIMINE_BOOTX64_SHA256 {
        bail!(
            "{} has sha256 {digest}, expected {LIMINE_BOOTX64_SHA256}; \
             delete {} and re-run to force a fresh checkout of {LIMINE_COMMIT}",
            bootx64.display(),
            dir.display()
        );
    }
    Ok(dir)
}

/// Runs git in `cwd`, appending `extra` after the argument list.
///
/// A path argument goes through `extra` rather than `args` because `OsStr` is
/// not necessarily UTF-8, and forcing it through `&str` would mangle it.
fn git(cwd: &Path, args: &[&str], extra: Option<&Path>) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd);
    cmd.args(args);
    if let Some(extra) = extra {
        cmd.arg(extra);
    }
    let status =
        cmd.status().with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

/// Copies `src` to `dst` unless a recorded stamp says the destination already
/// came from this exact source.
///
/// Not a comparison of `src` against `dst`: the stamp records the source path,
/// length and mtime, and `dst`'s length is checked separately. The reason is in
/// the comments below -- provenance, not just freshness.
///
/// The kernel ELF is tens of MiB with debug info, and BOOTX64.EFI plus
/// limine.conf never change during a dev session; re-copying all three on every
/// run is pure syscall cost.
fn copy_if_changed(src: &Path, dst: &Path, stamps: &Path) -> Result<()> {
    let src_meta = std::fs::metadata(src)
        .with_context(|| format!("reading source metadata for {}", src.display()))?;
    // Provenance, not just freshness. `run`, `run --release` and `runner` all
    // write the same destination from different sources, and those ELFs can be
    // identical in length -- so a (len, mtime) test alone can leave the
    // previous command's kernel in place and silently boot the wrong image.
    // Stamps live outside the ESP: anything inside it is both served to the
    // guest through VVFAT and swept by `prune_unexpected`.
    let stamp_name = dst.file_name().unwrap_or_default();
    let stamp_path = stamps.join(stamp_name).with_extension("stamp");
    // No mtime means no way to tell two same-length sources apart, so treat it
    // as a miss. Recording it as a literal `None` would instead make every
    // mtime-less source match every other one.
    let stamp = src_meta
        .modified()
        .ok()
        .map(|modified| format!("{}\n{}\n{modified:?}", src.display(), src_meta.len()));
    // The destination's length is checked too, not just its existence: a copy
    // interrupted partway leaves a truncated file that VVFAT serves happily,
    // and Limine then fails in a way that reads as a kernel bug.
    let fresh = stamp.as_ref().is_some_and(|stamp| {
        std::fs::read_to_string(&stamp_path).is_ok_and(|existing| existing == *stamp)
    }) && std::fs::metadata(dst).is_ok_and(|meta| meta.len() == src_meta.len());
    if fresh {
        return Ok(());
    }
    std::fs::copy(src, dst)
        .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
    match &stamp {
        Some(stamp) => std::fs::write(&stamp_path, stamp)
            .with_context(|| format!("writing stamp {}", stamp_path.display()))?,
        // Leaving the previous stamp in place would let a later run whose mtime
        // is readable match a stamp that describes a different source.
        None => match std::fs::remove_file(&stamp_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("clearing stale stamp {}", stamp_path.display())
                });
            }
        },
    }
    Ok(())
}

/// Assembles an EFI System Partition as a plain directory.
///
/// QEMU serves this directly with `-drive format=raw,file=fat:rw:<dir>`, so no
/// ISO or filesystem-image tooling is involved.
///
/// The tree is not torn down between runs; instead every file it should contain
/// is refreshed only when stale, and anything else present is removed. That
/// keeps VVFAT from serving leftovers while avoiding a full re-copy each run.
/// Sectors in the test disk. 2048 × 512 B = 1 MiB, which is more than any test
/// reads and small enough to regenerate in a blink.
const TEST_DISK_SECTORS: u64 = 2048;

/// Creates the guest's block device, if it is not already there.
///
/// **Every sector begins with its own LBA**, little-endian, and the rest is
/// filled with a byte derived from it. That is the whole point: a read of the
/// wrong sector is then *detectable* rather than plausible. A disk of zeros, or
/// of one repeated pattern, would let an off-by-one in the descriptor chain
/// return data that looks exactly like success.
///
/// Written only when absent. Regenerating it every run would erase whatever a
/// write test had just put there, so a read-after-write test could never fail
/// for the right reason -- and it would also make the two boots of
/// `cargo xtask test` disagree about the disk's contents.
pub fn build_test_disk(target_dir: &Path) -> Result<PathBuf> {
    let disk = target_dir.join("qunix-test-disk.img");
    // Regenerated when the *size* is wrong, not merely when the file is
    // missing. A write interrupted by a full disk or a killed run leaves a
    // truncated image that would then be served to the guest forever, and the
    // kernel's read tests only notice if they happen to read past its end --
    // which they mostly do not. The same check invalidates an image from an
    // older `TEST_DISK_SECTORS`.
    if disk.exists()
        && std::fs::metadata(&disk).map(|m| m.len()).unwrap_or(0) == TEST_DISK_SECTORS * 512
    {
        return Ok(disk);
    }
    std::fs::create_dir_all(target_dir)?;
    let image = generate_test_disk();
    std::fs::write(&disk, &image)
        .with_context(|| format!("failed to write the test disk at {}", disk.display()))?;
    Ok(disk)
}

/// The image's contents, without touching the filesystem.
///
/// Separated so the shape the kernel asserts from inside the emulator can be
/// asserted here too. Those two statements of one format live in different
/// crates and were checked against each other by nothing.
fn generate_test_disk() -> Vec<u8> {
    const SECTOR_BYTES: usize = 512;
    let mut image = vec![0u8; TEST_DISK_SECTORS as usize * SECTOR_BYTES];
    for lba in 0..TEST_DISK_SECTORS {
        let base = lba as usize * SECTOR_BYTES;
        image[base..base + 8].copy_from_slice(&lba.to_le_bytes());
        // A filler that also depends on the LBA, so a read returning the right
        // first eight bytes and the wrong tail is caught too.
        for (i, byte) in image[base + 8..base + SECTOR_BYTES].iter_mut().enumerate() {
            *byte = (lba as u8).wrapping_add(i as u8);
        }
    }
    image
}

#[cfg(test)]
mod disk_tests {
    use super::*;

    #[test]
    fn every_sector_identifies_itself() {
        // The kernel's block tests assert this exact shape from the other side
        // of the emulator, and nothing connected the two statements. This is
        // the half that can be checked on the host.
        let image = generate_test_disk();
        assert_eq!(image.len(), TEST_DISK_SECTORS as usize * 512);
        for lba in [0u64, 1, 7, 11, TEST_DISK_SECTORS - 1] {
            let s = &image[lba as usize * 512..][..512];
            assert_eq!(
                u64::from_le_bytes(s[0..8].try_into().unwrap()),
                lba,
                "sector {lba} does not begin with its own LBA"
            );
            assert_eq!(s[8], lba as u8, "the filler does not start at the LBA");
            assert_eq!(s[9], (lba as u8).wrapping_add(1), "the filler does not advance");
        }
        // And no two sectors are identical, which is what makes reading the
        // wrong one detectable rather than plausible -- the property the whole
        // generator exists for.
        assert_ne!(&image[0..512], &image[512..1024]);
    }
}

pub fn build_esp(root: &Path, target_dir: &Path, kernel: &Path) -> Result<PathBuf> {
    let limine = ensure_limine(root)?;
    let esp = target_dir.join("esp");
    std::fs::create_dir_all(esp.join("EFI/BOOT"))?;
    std::fs::create_dir_all(esp.join("boot/limine"))?;

    let stamps = target_dir.join("esp-stamps");
    std::fs::create_dir_all(&stamps)?;

    copy_if_changed(&limine.join("BOOTX64.EFI"), &esp.join("EFI/BOOT/BOOTX64.EFI"), &stamps)?;
    copy_if_changed(&root.join("limine.conf"), &esp.join("boot/limine/limine.conf"), &stamps)?;
    copy_if_changed(kernel, &esp.join("boot/qunix-kernel"), &stamps)?;
    // The init program is a separate ELF the bootloader hands the kernel as a
    // module, not something linked into the kernel image.
    let init = crate::userland::build_init(root, target_dir)?;
    copy_if_changed(&init, &esp.join("boot/init.elf"), &stamps)?;

    prune_unexpected(&esp)?;
    Ok(esp)
}

/// Removes anything in the ESP that `build_esp` did not place there, so a stale
/// artefact cannot reach the guest through VVFAT.
///
/// `EXPECTED` is coupled to `build_esp` by hand: a `copy_if_changed`
/// destination that is not listed here is written and then deleted in the same
/// call, and the failure surfaces as a bootloader error rather than a build
/// one. Keep the two in step.
fn prune_unexpected(esp: &Path) -> Result<()> {
    const EXPECTED: &[&str] = &[
        "EFI",
        "EFI/BOOT",
        "EFI/BOOT/BOOTX64.EFI",
        "boot",
        "boot/limine",
        "boot/limine/limine.conf",
        "boot/qunix-kernel",
        "boot/init.elf",
        // Written by OVMF itself. Deleting it makes the firmware redo its
        // variable-store init on every boot.
        "NvVars",
    ];
    // This is the only recursive delete in the tree, so it refuses to start
    // anywhere but the directory it is meant for: a `target_dir()` that somehow
    // resolved elsewhere must not turn into `rm -rf` of that place.
    assert!(esp.ends_with("esp"), "refusing to prune {}: not an esp directory", esp.display());
    if std::fs::symlink_metadata(esp)
        .with_context(|| format!("stat-ing ESP root {}", esp.display()))?
        .file_type()
        .is_symlink()
    {
        bail!("refusing to prune {}: it is a symlink", esp.display());
    }
    fn walk(dir: &Path, base: &Path, expected: &[&str]) -> Result<()> {
        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("reading ESP directory {}", dir.display()))?;
        for entry in entries {
            let entry =
                entry.with_context(|| format!("reading ESP directory {}", dir.display()))?;
            let path = entry.path();
            let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().to_string();
            let is_dir = entry
                .file_type()
                .with_context(|| format!("stat-ing ESP entry {}", path.display()))?
                .is_dir();
            if !expected.contains(&rel.as_str()) {
                let removed = if is_dir {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                removed.with_context(|| {
                    format!("pruning unexpected ESP entry {}", path.display())
                })?;
                continue;
            }
            if is_dir {
                walk(&path, base, expected)?;
            }
        }
        Ok(())
    }
    walk(esp, esp, EXPECTED)
}

/// SHA-256 of a file, as lowercase hex.
///
/// Implemented here rather than pulled in as a dependency: it is one fixed
/// algorithm used in exactly one place, and a build tool whose job is to verify
/// a bootloader should not widen its own supply chain to do so.
fn sha256_file(path: &Path) -> Result<String> {
    let data =
        std::fs::read(path).with_context(|| format!("reading {} to hash", path.display()))?;
    Ok(sha256_hex(&data))
}

fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // The message length is appended as a count of bits, and the padding runs
    // to 56 mod 64 so that count lands at the end of a whole block.
    let mut padded = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let (blocks, _) = padded.as_chunks::<64>();
    for block in blocks {
        let mut w = [0u32; 64];
        let (words, _) = block.as_chunks::<4>();
        for (schedule, word) in w.iter_mut().zip(words) {
            *schedule = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for (k, schedule) in K.iter().zip(w.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(*schedule);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (state, round) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *state = state.wrapping_add(round);
        }
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::sha256_hex;

    /// The published vectors. A wrong implementation would otherwise surface
    /// only as a bootloader that never verifies, long after the fact.
    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 56 bytes is exactly where the length field no longer fits in the
        // first block, so this is the case that exercises multi-block padding.
        assert_eq!(
            sha256_hex(&[b'a'; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
    }
}
