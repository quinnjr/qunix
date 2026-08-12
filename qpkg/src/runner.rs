//! Runs the PKGBUILD's functions — `prepare`, `build`, `package`, in that
//! order — each in its own scrubbed bash, and then checks that the package
//! tree cannot reach outside itself. There is no fakeroot: `pkgdir` is an
//! ordinary directory and ownership is normalized when the archive is
//! written, not here.

use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::pkgbuild::Pkgbuild;

const STAGES: &[&str] = &["prepare", "build", "package"];

pub fn run_functions(
    pb: &Pkgbuild,
    pkgbuild_path: &Path,
    workdir: &Path,
    env: &[(String, String)],
) -> Result<()> {
    let srcdir = workdir.join("src");
    let pkgdir = workdir.join("pkg");
    std::fs::create_dir_all(&srcdir)?;
    std::fs::create_dir_all(&pkgdir)?;
    let log_path = workdir.join("build.log");
    let mut log = std::fs::File::create(&log_path)?;

    for stage in STAGES.iter().filter(|s| pb.functions.contains(**s)) {
        // `"$2"` invokes the sourced function by name; `set -e` makes any
        // failing command inside it fail the stage rather than scroll past.
        writeln!(log, "==> {stage}()")?;
        log.flush()?;
        // Both streams write straight into the log file: a verbose build can
        // emit tens of MB per stage, none of which needs to sit in memory,
        // and the log fills as the stage runs instead of at its end.
        let status = Command::new("bash")
            .args(["--noprofile", "--norc", "-c", r#"set -e; source "$1"; cd "$srcdir"; "$2""#, "qpkg-run"])
            .arg(pkgbuild_path)
            .arg(stage)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .env("srcdir", &srcdir)
            .env("pkgdir", &pkgdir)
            .env("pkgname", pb.pkgname.first().map(String::as_str).unwrap_or_default())
            .env("pkgver", &pb.pkgver)
            .env("pkgrel", &pb.pkgrel)
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
            .status()
            .map_err(|e| Error::Extraction(format!("spawning bash for {stage}(): {e}")))?;
        if !status.success() {
            return Err(Error::Build { stage: stage_name(stage), log: log_path });
        }
    }
    Ok(())
}

/// `Error::Build` carries a `&'static str`; this pins each stage's name to
/// the one static spelling.
fn stage_name(stage: &str) -> &'static str {
    match stage {
        "prepare" => "prepare",
        "build" => "build",
        _ => "package",
    }
}

/// Refuses any symlink in `pkgdir` whose target resolves outside it: an
/// absolute link to the host, or a relative one climbing out. The archive
/// records link targets verbatim, and whatever extracts it on the qunix side
/// must never be handed a path that was already outside the package.
pub fn check_containment(pkgdir: &Path) -> Result<()> {
    walk(pkgdir, pkgdir)
}

fn walk(pkgdir: &Path, dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?;
            if !resolves_inside(pkgdir, dir, &target) {
                return Err(Error::Containment(path));
            }
        } else if meta.is_dir() {
            walk(pkgdir, &path)?;
        }
    }
    Ok(())
}

/// Lexical resolution — no filesystem access — of a symlink target found in
/// `parent`, asking whether it stays under `pkgdir`.
fn resolves_inside(pkgdir: &Path, parent: &Path, target: &Path) -> bool {
    let mut resolved: PathBuf = if target.is_absolute() {
        PathBuf::new()
    } else {
        parent.to_path_buf()
    };
    for component in target.components() {
        match component {
            Component::ParentDir => {
                if !resolved.pop() {
                    return false;
                }
            }
            Component::CurDir => {}
            other => resolved.push(other),
        }
    }
    resolved.starts_with(pkgdir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::os::unix::fs::symlink;

    fn minimal_env(workdir: &Path) -> Vec<(String, String)> {
        vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), workdir.display().to_string()),
            ("LC_ALL".into(), "C".into()),
        ]
    }

    fn pb_with(functions: &[&str]) -> Pkgbuild {
        Pkgbuild {
            pkgname: vec!["t".into()],
            pkgver: "1".into(),
            pkgrel: "1".into(),
            functions: functions.iter().map(|f| f.to_string()).collect::<BTreeSet<_>>(),
            ..Default::default()
        }
    }

    fn write_pkgbuild(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("PKGBUILD");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn stages_run_in_order_and_package_lands_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pkgbuild(
            dir.path(),
            concat!(
                "pkgname=t\npkgver=1\npkgrel=1\n",
                "prepare() { echo prepared > \"$srcdir/trail\"; }\n",
                "build() { echo built >> \"$srcdir/trail\"; }\n",
                "package() { mkdir -p \"$pkgdir/usr/bin\"; cp \"$srcdir/trail\" \"$pkgdir/usr/bin/hello\"; }\n",
            ),
        );
        let pb = pb_with(&["prepare", "build", "package"]);
        run_functions(&pb, &path, dir.path(), &minimal_env(dir.path())).unwrap();
        // Order is observable: package copied what prepare and build wrote.
        let out = std::fs::read_to_string(dir.path().join("pkg/usr/bin/hello")).unwrap();
        assert_eq!(out, "prepared\nbuilt\n");
    }

    #[test]
    fn absent_stages_are_skipped_not_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pkgbuild(
            dir.path(),
            "pkgname=t\npkgver=1\npkgrel=1\npackage() { touch \"$pkgdir/only\"; }\n",
        );
        // functions says only package() exists; no phantom prepare/build runs.
        let pb = pb_with(&["package"]);
        run_functions(&pb, &path, dir.path(), &minimal_env(dir.path())).unwrap();
        assert!(dir.path().join("pkg/only").exists());
    }

    #[test]
    fn a_failing_stage_names_itself_and_leaves_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pkgbuild(
            dir.path(),
            concat!(
                "pkgname=t\npkgver=1\npkgrel=1\n",
                "build() { echo 'the compiler exploded' >&2; false; }\n",
                "package() { :; }\n",
            ),
        );
        let pb = pb_with(&["build", "package"]);
        let err = run_functions(&pb, &path, dir.path(), &minimal_env(dir.path())).unwrap_err();
        match err {
            Error::Build { stage, log } => {
                assert_eq!(stage, "build");
                let text = std::fs::read_to_string(log).unwrap();
                assert!(text.contains("the compiler exploded"), "{text}");
            }
            other => panic!("expected Build, got {other:?}"),
        }
        // Negative: package() never ran after the failure.
        assert!(!dir.path().join("pkg").join("only").exists());
    }

    #[test]
    fn containment_judges_symlinks_lexically() {
        let dir = tempfile::tempdir().unwrap();
        let pkgdir = dir.path().join("pkg");
        std::fs::create_dir_all(pkgdir.join("usr/bin")).unwrap();
        std::fs::write(pkgdir.join("usr/bin/real"), "x").unwrap();

        // Internal relative link: fine.
        symlink("./real", pkgdir.join("usr/bin/alias")).unwrap();
        check_containment(&pkgdir).unwrap();

        // Relative escape: refused, naming the link.
        symlink("../../../../etc", pkgdir.join("usr/bin/up")).unwrap();
        match check_containment(&pkgdir).unwrap_err() {
            Error::Containment(p) => assert!(p.ends_with("usr/bin/up"), "{}", p.display()),
            other => panic!("expected Containment, got {other:?}"),
        }
        std::fs::remove_file(pkgdir.join("usr/bin/up")).unwrap();

        // Absolute link to the host: refused.
        symlink("/etc/passwd", pkgdir.join("usr/bin/abs")).unwrap();
        assert!(matches!(check_containment(&pkgdir).unwrap_err(), Error::Containment(_)));
    }
}
