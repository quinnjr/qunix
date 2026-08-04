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
            let expected = zone_for(&name);

            // `license.workspace = true` resolves to the workspace value, which
            // is the permissive licence. That is only acceptable for a crate
            // whose zone IS permissive.
            let declared = if manifest.contains("license.workspace = true") {
                PERMISSIVE.to_string()
            } else {
                match manifest_field(&manifest, "license") {
                    Some(license) => license,
                    None => bail!("{name} declares no license", name = name),
                }
            };

            if declared != expected {
                bail!(
                    "{name} is licensed {declared:?} but its zone requires {expected:?} \
                     (see LICENSING.md). A Linux-compatibility crate must declare \
                     `license = \"{COPYLEFT}\"` explicitly, not inherit the workspace default."
                );
            }
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
