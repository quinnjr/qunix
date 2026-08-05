//! Enforces the per-crate licensing split described in `LICENSING.md`.
//!
//! Every crate declares `license.workspace = true`, which resolves to the
//! permissive licence. That is right for qunix's own code and wrong for the
//! Linux compatibility layer — meaning a new `qunix-linux-compat` would inherit
//! a permissive licence by doing nothing at all. This check makes that silence
//! a build failure instead.

use anyhow::{Result, bail};
use std::path::Path;

const PERMISSIVE: &str = "MIT OR Apache-2.0";
const COPYLEFT: &str = "GPL-2.0";

/// Crates whose names put them in the copyleft zone.
///
/// Matched as substrings so `qunix-linux-compat`, `linux-shim`, and anything
/// like `qunix-linux-headers` are all caught without needing this list updated
/// first — the failure mode of a new crate should be "build stops", not
/// "ships permissive".
const COPYLEFT_MARKERS: &[&str] = &["linux-compat", "linux-shim", "linux-headers"];

/// The syscall personality is NOT copyleft: matching UAPI struct layouts so
/// unmodified binaries run is a different thing from reimplementing in-kernel
/// driver interfaces. Without this exemption the `linux` substring would sweep
/// it in.
const PERMISSIVE_EXCEPTIONS: &[&str] = &["qunix-linux-abi"];

fn zone_for(crate_name: &str) -> &'static str {
    if PERMISSIVE_EXCEPTIONS.contains(&crate_name) {
        return PERMISSIVE;
    }
    if COPYLEFT_MARKERS.iter().any(|m| crate_name.contains(m)) {
        return COPYLEFT;
    }
    PERMISSIVE
}

/// Reads a `name = "..."` / `license = "..."` pair out of a manifest.
///
/// Deliberately a line scan rather than a TOML dependency: this runs on every
/// `xtask test`, and the shapes it needs are two keys in `[package]`.
fn manifest_field(manifest: &str, key: &str) -> Option<String> {
    manifest
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(key)?.trim().strip_prefix('=').map(str::trim))
        .map(|value| value.trim_matches('"').to_string())
}

/// The licence a manifest effectively declares.
///
/// `license.workspace = true` resolves to the workspace value, which is the
/// permissive licence. That is only acceptable for a crate whose zone IS
/// permissive, and treating it as "unset" rather than as permissive is what
/// would let a Linux-compat crate ship permissive by declaring nothing.
fn declared_license(manifest: &str) -> Option<String> {
    if manifest.contains("license.workspace = true") {
        return Some(PERMISSIVE.to_string());
    }
    manifest_field(manifest, "license")
}

/// The verdict for one crate, given its name and its manifest text.
///
/// Split from the directory walk because the walk is I/O and the verdict is the
/// rule. Until this existed the tests reached only `zone_for`, so nothing
/// asserted the thing the check is for: that a Linux-compat crate inheriting
/// the workspace licence is **rejected**. Every test was in the accepting
/// direction, and deleting the comparison in `check` would have passed all of
/// them.
fn verdict(name: &str, manifest: &str) -> Result<()> {
    let expected = zone_for(name);
    let Some(declared) = declared_license(manifest) else {
        bail!("{name} declares no license");
    };
    if declared != expected {
        bail!(
            "{name} is licensed {declared:?} but its zone requires {expected:?} \
             (see LICENSING.md). A Linux-compatibility crate must declare \
             `license = \"{COPYLEFT}\"` explicitly, not inherit the workspace default."
        );
    }
    Ok(())
}

/// Fails if any crate's declared licence does not match its zone.
pub fn check(root: &Path) -> Result<()> {
    let mut checked = 0usize;
    for dir in ["crates", "."] {
        let base = root.join(dir);
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries {
            let manifest_path = entry?.path().join("Cargo.toml");
            if !manifest_path.is_file() {
                continue;
            }
            let manifest = std::fs::read_to_string(&manifest_path)?;
            let Some(name) = manifest_field(&manifest, "name") else {
                continue; // virtual manifest, no [package]
            };
            // The rule itself lives in `verdict`, which is testable without a
            // directory to walk. Inlining it here as well would be two copies
            // of the same comparison that must agree -- the shape of defect
            // this project keeps finding.
            verdict(&name, &manifest)?;
            checked += 1;
        }
    }
    if checked == 0 {
        bail!("licensing check found no crates to inspect; the workspace layout changed");
    }
    println!("licensing: {checked} crates match their zone");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{COPYLEFT, PERMISSIVE, verdict};

    /// A manifest with the two keys the checker reads.
    fn manifest(name: &str, license_line: &str) -> String {
        format!("[package]\nname = \"{name}\"\n{license_line}\nedition = \"2024\"\n")
    }

    #[test]
    fn a_linux_compat_crate_inheriting_the_workspace_licence_is_rejected() {
        // The whole reason the check exists, and until now nothing asserted it.
        // `license.workspace = true` resolves to the permissive licence, so a
        // Linux-compat crate that simply says nothing would ship permissive --
        // silently, because every existing test was in the accepting direction
        // and deleting the comparison in `check` passed all of them.
        let m = manifest("qunix-linux-ext4", "license.workspace = true");
        let err = verdict("qunix-linux-ext4", &m).unwrap_err().to_string();
        assert!(err.contains("qunix-linux-ext4"), "{err}");
        assert!(err.contains(COPYLEFT), "the error does not name the required licence: {err}");
    }

    #[test]
    fn a_linux_compat_crate_declaring_permissive_is_rejected() {
        // Explicitly wrong rather than merely unset. Both must fail, or the
        // guard covers one of two ways to get it wrong.
        let m = manifest("qunix-linux-drm", &format!("license = \"{PERMISSIVE}\""));
        assert!(verdict("qunix-linux-drm", &m).is_err());
    }

    #[test]
    fn a_permissive_crate_declaring_copyleft_is_rejected() {
        // The opposite direction is also a violation: qunix's own code must not
        // silently become GPL, which would relicense the project by accident.
        let m = manifest("qunix-mm", &format!("license = \"{COPYLEFT}\""));
        assert!(verdict("qunix-mm", &m).is_err());
    }

    #[test]
    fn a_crate_declaring_no_licence_at_all_is_rejected() {
        let m = "[package]\nname = \"qunix-mm\"\nedition = \"2024\"\n";
        let err = verdict("qunix-mm", m).unwrap_err().to_string();
        assert!(err.contains("declares no license"), "{err}");
    }

    #[test]
    fn the_deliberate_exemption_is_exactly_one_crate() {
        // `qunix-linux-abi` is exempt because matching UAPI struct layouts is
        // not reimplementing the in-kernel driver API (see LICENSING.md). The
        // risk is that the exemption widens by prefix match, taking every
        // future `qunix-linux-*` crate with it.
        assert!(verdict("qunix-linux-abi", &manifest("qunix-linux-abi", "license.workspace = true")).is_ok());
        assert!(
            verdict("qunix-linux-abi-helpers", &manifest("qunix-linux-abi-helpers", "license.workspace = true")).is_err(),
            "the exemption widened to a crate that merely starts with the exempt name"
        );
    }

    #[test]
    fn crates_in_each_zone_are_accepted_when_correct() {
        assert!(verdict("qunix-mm", &manifest("qunix-mm", "license.workspace = true")).is_ok());
        assert!(
            verdict("qunix-linux-ext4", &manifest("qunix-linux-ext4", &format!("license = \"{COPYLEFT}\""))).is_ok()
        );
    }

    use super::*;

    #[test]
    fn linux_compat_crates_land_in_the_copyleft_zone() {
        assert_eq!(zone_for("qunix-linux-compat"), COPYLEFT);
        assert_eq!(zone_for("linux-shim"), COPYLEFT);
        assert_eq!(zone_for("qunix-linux-headers"), COPYLEFT);
    }

    #[test]
    fn qunix_own_crates_stay_permissive() {
        for name in ["qunix-mm", "qunix-sync", "qunix-hal-x86_64", "qunix-kernel", "xtask"] {
            assert_eq!(zone_for(name), PERMISSIVE, "{name}");
        }
    }

    #[test]
    fn the_syscall_personality_is_not_copyleft() {
        // Matching UAPI layouts is not the same as reimplementing the driver API.
        assert_eq!(zone_for("qunix-linux-abi"), PERMISSIVE);
    }

    #[test]
    fn workspace_inheritance_is_read_as_permissive() {
        let manifest = "[package]\nname = \"qunix-mm\"\nlicense.workspace = true\n";
        assert_eq!(manifest_field(manifest, "name").as_deref(), Some("qunix-mm"));
        assert!(manifest.contains("license.workspace = true"));
    }
}
