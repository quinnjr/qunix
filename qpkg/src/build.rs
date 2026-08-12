//! The full pipeline for one package: index lookup → PKGBUILD fetch →
//! rewrite → extract → arch check → stage sources → run functions → contain
//! → package → record. The `built` table is written exactly once, at the very
//! end — every earlier failure leaves it untouched, so it can never claim
//! success for a build that did not finish.

use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::Digest as _;

use crate::error::{Error, Result};
use crate::index::{BuiltRecord, Index, PackageRecord, Repo};
use crate::sources::{self, Fetch};
use crate::toolchain::{self, RewriteLog};
use crate::vercmp::vercmp;
use crate::{artifact, paths, pkgbuild, runner, sync};

/// CLI entry: real network, real toolchain check, real clock.
pub fn run(index: &Index, name: &str, no_rewrite: bool, now: u64) -> Result<PathBuf> {
    toolchain::check_host(Path::new(toolchain::MUSL_ROOT))?;
    let workdir = paths::build_dir(name);
    let env = toolchain::build_env(&workdir);
    let (path, rewrites) = build_inner(
        index,
        name,
        no_rewrite,
        &sync::http::fetch,
        &paths::artifacts_dir(),
        &workdir,
        &env,
        now,
    )?;
    for r in &rewrites {
        eprintln!("rewrote line {} ({}): {} => {}", r.line, r.rule, r.before.trim(), r.after.trim());
    }
    Ok(path)
}

/// The testable core: network and environment injected.
#[allow(clippy::too_many_arguments)]
pub fn build_inner(
    index: &Index,
    name: &str,
    no_rewrite: bool,
    fetch: Fetch,
    out_dir: &Path,
    workdir: &Path,
    env: &[(String, String)],
    now: u64,
) -> Result<(PathBuf, Vec<RewriteLog>)> {
    let rec = index
        .get(name)?
        .ok_or_else(|| Error::Index(format!("{name} is not in the index (try `qpkg sync`)")))?;
    std::fs::create_dir_all(workdir)?;

    // pkgbuild_dir is where local `source=` files live: the snapshot checkout
    // for AUR packages, the workdir itself (populated below) for official.
    let (pkgbuild_bytes, pkgbuild_dir) = fetch_pkgbuild(&rec, workdir, fetch)?;
    let sha = hex::encode(sha2::Sha256::digest(&pkgbuild_bytes));
    let text = String::from_utf8_lossy(&pkgbuild_bytes).into_owned();
    let (text, rewrites) =
        if no_rewrite { (text, Vec::new()) } else { toolchain::rewrite(&text) };
    let pkgbuild_path = pkgbuild_dir.join("PKGBUILD");
    std::fs::write(&pkgbuild_path, &text)?;

    let pb = pkgbuild::extract(&pkgbuild_path, workdir)?;
    pb.check_arch()?;

    // Official repos serve each packaging file individually; pull the local
    // sources down beside the PKGBUILD so staging finds them.
    if rec.repo != Repo::Aur {
        for entry in &pb.source {
            let file = entry.split_once("::").map(|(d, _)| d).unwrap_or(entry.as_str());
            if !entry.contains("://") && !entry.starts_with("git+") {
                let url = official_raw_url(&rec.package_base, &rec.version, file);
                let bytes = fetch(&url)?;
                refuse_html(&url, &bytes)?;
                std::fs::write(pkgbuild_dir.join(file), bytes)?;
            }
        }
    }

    sources::stage(&pb, &pkgbuild_dir, workdir, fetch)?;
    runner::run_functions(&pb, &pkgbuild_path, workdir, env)?;
    runner::check_containment(&workdir.join("pkg"))?;
    let artifact_path = artifact::package(&pb, &workdir.join("pkg"), out_dir)?;

    index.record_built(&BuiltRecord {
        name: name.to_string(),
        version_built: pb.full_version(),
        pkgbuild_sha256: sha,
        artifact_path: artifact_path.clone(),
        built_at_unix: now,
    })?;
    Ok((artifact_path, rewrites))
}

fn fetch_pkgbuild(rec: &PackageRecord, workdir: &Path, fetch: Fetch) -> Result<(Vec<u8>, PathBuf)> {
    match rec.repo {
        Repo::Core | Repo::Extra => {
            let url = official_raw_url(&rec.package_base, &rec.version, "PKGBUILD");
            let bytes = fetch(&url)?;
            refuse_html(&url, &bytes)?;
            Ok((bytes, workdir.to_path_buf()))
        }
        Repo::Aur => {
            // The snapshot tarball carries the PKGBUILD *and* its local
            // support files; unpack the whole thing.
            let bytes = fetch(&crate::aur::snapshot_url(&rec.package_base))?;
            let gz = flate2::read::GzDecoder::new(bytes.as_slice());
            let mut archive = tar::Archive::new(gz);
            archive
                .unpack(workdir)
                .map_err(|e| Error::Extraction(format!("AUR snapshot: {e}")))?;
            let dir = workdir.join(&rec.package_base);
            let pkgbuild = std::fs::read(dir.join("PKGBUILD")).map_err(|e| {
                Error::Extraction(format!("AUR snapshot has no PKGBUILD for {}: {e}", rec.name))
            })?;
            Ok((pkgbuild, dir))
        }
    }
}

/// Package names GitLab reserves as route words; Arch's packaging repos
/// carry them under a `unix-` prefix (`tree` lives at `unix-tree`). The set
/// is GitLab's documented reserved-name list, filtered to plausible package
/// names.
const GITLAB_RESERVED: &[&str] =
    &["badges", "blame", "blob", "builds", "commits", "create", "edit", "environments",
      "files", "new", "preview", "raw", "refs", "tree", "update", "wikis"];

/// The Arch GitLab packaging layout. Project paths and tags mangle what
/// GitLab refuses: `+` becomes `plus` in the project name, `:` (epoch)
/// becomes `-` in the tag, and reserved route words gain a `unix-` prefix.
fn official_raw_url(base: &str, version: &str, file: &str) -> String {
    let mut project = base.replace('+', "plus");
    if GITLAB_RESERVED.contains(&project.as_str()) {
        project = format!("unix-{project}");
    }
    let tag = version.replace(':', "-");
    format!("https://gitlab.archlinux.org/archlinux/packaging/packages/{project}/-/raw/{tag}/{file}")
}

/// A GitLab miss can 302 to the sign-in page, which arrives as 200 HTML —
/// without this check it surfaces later as a baffling bash syntax error.
fn refuse_html(url: &str, bytes: &[u8]) -> Result<()> {
    let head = bytes.get(..64).unwrap_or(bytes);
    let head = String::from_utf8_lossy(head);
    let head = head.trim_start();
    if head.starts_with("<!DOCTYPE") || head.starts_with("<html") {
        return Err(Error::Network(format!(
            "{url} returned an HTML page, not a file — the packaging repo \
             probably lives under a different project name"
        )));
    }
    Ok(())
}

/// After a sync: every built package whose indexed version now sorts above
/// the built one. The caller decides whether reporting turns into rebuilding.
pub fn outdated(index: &Index, out: &mut impl Write) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for (built, upstream) in index.built_with_upstream()? {
        let Some(rec) = upstream else {
            // Dropped from the repos entirely; not an update.
            continue;
        };
        if vercmp(&rec.version, &built.version_built) == std::cmp::Ordering::Greater {
            writeln!(out, "{}: {} -> {}", built.name, built.version_built, rec.version)?;
            names.push(built.name);
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{gzipped, tar_bytes};

    fn minimal_env(workdir: &Path) -> Vec<(String, String)> {
        vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), workdir.display().to_string()),
            ("LC_ALL".into(), "C".into()),
        ]
    }

    fn seeded(repo: Repo, version: &str) -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("i.redb")).unwrap();
        index
            .upsert_packages(&[PackageRecord {
                name: "hello".into(),
                package_base: "hello".into(),
                version: version.into(),
                repo,
                description: String::new(),
                url: String::new(),
                depends: vec![],
                makedepends: vec![],
            }])
            .unwrap();
        (dir, index)
    }

    const GOOD_PKGBUILD: &str = concat!(
        "pkgname=hello\npkgver=1\npkgrel=2\narch=(any)\n",
        "package() { mkdir -p \"$pkgdir/usr/bin\"; echo hi > \"$pkgdir/usr/bin/hello\"; }\n",
    );

    fn build_hello(
        index: &Index,
        dir: &Path,
        pkgbuild: &'static str,
        no_rewrite: bool,
    ) -> Result<(PathBuf, Vec<RewriteLog>)> {
        let workdir = dir.join("work");
        let env = minimal_env(&workdir);
        build_inner(
            index,
            "hello",
            no_rewrite,
            &move |url| {
                assert!(
                    url.starts_with("https://gitlab.archlinux.org/archlinux/packaging/packages/hello/-/raw/1-2/"),
                    "official fetch goes to the version tag: {url}"
                );
                Ok(pkgbuild.as_bytes().to_vec())
            },
            &dir.join("artifacts"),
            &workdir,
            &env,
            42,
        )
    }

    #[test]
    fn a_successful_build_lands_the_artifact_and_the_built_row() {
        let (dir, index) = seeded(Repo::Core, "1-2");
        let (artifact, _) = build_hello(&index, dir.path(), GOOD_PKGBUILD, false).unwrap();
        assert!(artifact.exists());
        let built = index.built("hello").unwrap().unwrap();
        assert_eq!(built.version_built, "1-2");
        assert_eq!(built.built_at_unix, 42);
        // The recorded hash is of the *fetched* bytes, pre-rewrite.
        assert_eq!(
            built.pkgbuild_sha256,
            hex::encode(sha2::Sha256::digest(GOOD_PKGBUILD.as_bytes()))
        );
    }

    #[test]
    fn a_failed_build_leaves_no_built_row() {
        let (dir, index) = seeded(Repo::Core, "1-2");
        const BAD: &str =
            "pkgname=hello\npkgver=1\npkgrel=2\narch=(any)\nbuild() { false; }\npackage() { :; }\n";
        let err = build_hello(&index, dir.path(), BAD, false).unwrap_err();
        assert!(matches!(err, Error::Build { stage: "build", .. }), "{err:?}");
        // The spec's rule: never claim success for a failed build.
        assert!(index.built("hello").unwrap().is_none());
    }

    #[test]
    fn no_rewrite_skips_the_rewrite_pass() {
        const HARDCODED: &str = concat!(
            "pkgname=hello\npkgver=1\npkgrel=2\narch=(any)\n",
            "package() { mkdir -p \"$pkgdir/b\"; echo 'gcc -o x' > \"$pkgdir/b/cmd\"; }\n",
        );
        let (dir, index) = seeded(Repo::Core, "1-2");
        let (_, rewrites) = build_hello(&index, dir.path(), HARDCODED, false).unwrap();
        assert_eq!(rewrites.len(), 1, "the hardcoded gcc was rewritten: {rewrites:?}");

        let (dir, index) = seeded(Repo::Core, "1-2");
        let (_, rewrites) = build_hello(&index, dir.path(), HARDCODED, true).unwrap();
        // Negative: --no-rewrite leaves the PKGBUILD byte-identical.
        assert!(rewrites.is_empty());
        let text = std::fs::read_to_string(dir.path().join("work/PKGBUILD")).unwrap();
        assert_eq!(text, HARDCODED);
    }

    #[test]
    fn an_aur_package_builds_from_its_snapshot_with_local_sources() {
        let (dir, index) = seeded(Repo::Aur, "1-2");
        let patch = "patched content\n";
        let patch_sha = hex::encode(sha2::Sha256::digest(patch.as_bytes()));
        let pkgbuild = format!(
            concat!(
                "pkgname=hello\npkgver=1\npkgrel=2\narch=(any)\n",
                "source=(local.patch)\nsha256sums=('{sha}')\n",
                "package() {{ mkdir -p \"$pkgdir/usr\"; cp \"$srcdir/local.patch\" \"$pkgdir/usr/f\"; }}\n",
            ),
            sha = patch_sha
        );
        let snapshot = gzipped(&tar_bytes(&[
            ("hello/PKGBUILD", &pkgbuild),
            ("hello/local.patch", patch),
        ]));
        let workdir = dir.path().join("work");
        let env = minimal_env(&workdir);
        let snapshot_clone = snapshot.clone();
        let (artifact, _) = build_inner(
            &index,
            "hello",
            false,
            &move |url| {
                assert_eq!(url, "https://aur.archlinux.org/cgit/aur.git/snapshot/hello.tar.gz");
                Ok(snapshot_clone.clone())
            },
            &dir.path().join("artifacts"),
            &workdir,
            &env,
            7,
        )
        .unwrap();
        assert!(artifact.exists());
        // The local source came out of the snapshot, checksum-verified.
        assert_eq!(index.built("hello").unwrap().unwrap().version_built, "1-2");
    }

    #[test]
    fn gitlab_names_mangle_reserved_words_pluses_and_epochs() {
        assert_eq!(
            official_raw_url("tree", "2.3.2-1", "PKGBUILD"),
            "https://gitlab.archlinux.org/archlinux/packaging/packages/unix-tree/-/raw/2.3.2-1/PKGBUILD"
        );
        assert!(official_raw_url("libsigc++", "3.6-1", "PKGBUILD").contains("/libsigcplusplus/"));
        assert!(official_raw_url("zlib", "1:1.3.2-3", "PKGBUILD").contains("/raw/1-1.3.2-3/"));
        // Negative: an unreserved name passes through unprefixed.
        assert!(official_raw_url("zsh", "5.9-5", "PKGBUILD").contains("/packages/zsh/-/"));
    }

    #[test]
    fn an_html_answer_is_refused_with_a_real_message() {
        let (dir, index) = seeded(Repo::Core, "1-2");
        let workdir = dir.path().join("work");
        let env = minimal_env(&workdir);
        let err = build_inner(
            &index,
            "hello",
            false,
            &|_| Ok(b"<!DOCTYPE html>\n<html>sign in please</html>".to_vec()),
            &dir.path().join("artifacts"),
            &workdir,
            &env,
            1,
        )
        .unwrap_err();
        match err {
            Error::Network(msg) => assert!(msg.contains("HTML"), "{msg}"),
            other => panic!("expected Network, got {other:?}"),
        }
        // And no built row, as always on failure.
        assert!(index.built("hello").unwrap().is_none());
    }

    #[test]
    fn outdated_lists_only_genuinely_newer_upstreams() {
        let (dir, index) = seeded(Repo::Extra, "1-2");
        build_hello(&index, dir.path(), GOOD_PKGBUILD, false).unwrap();
        // Upstream then moves to 1-3.
        index
            .upsert_packages(&[PackageRecord {
                name: "hello".into(),
                package_base: "hello".into(),
                version: "1-3".into(),
                repo: Repo::Extra,
                description: String::new(),
                url: String::new(),
                depends: vec![],
                makedepends: vec![],
            }])
            .unwrap();
        let mut out = Vec::new();
        let names = outdated(&index, &mut out).unwrap();
        assert_eq!(names, ["hello"]);
        assert!(String::from_utf8(out).unwrap().contains("hello: 1-2 -> 1-3"));

        // Negative: once the index matches the built version, silence.
        index
            .upsert_packages(&[PackageRecord {
                name: "hello".into(),
                package_base: "hello".into(),
                version: "1-2".into(),
                repo: Repo::Extra,
                description: String::new(),
                url: String::new(),
                depends: vec![],
                makedepends: vec![],
            }])
            .unwrap();
        let mut out = Vec::new();
        assert!(outdated(&index, &mut out).unwrap().is_empty());
        assert!(out.is_empty());
    }
}
