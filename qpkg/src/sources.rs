//! Staging of `source=` entries: download, verify, extract — all in Rust,
//! precisely so a PKGBUILD cannot skip its own checksum verification. The
//! network edge is an injected closure; nothing here dials out on its own.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use blake2::Digest as _;

use crate::error::{Error, Result};
use crate::pkgbuild::Pkgbuild;

pub type Fetch<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>>;

enum Sums<'a> {
    Sha256(&'a [String]),
    B2(&'a [String]),
    None,
}

/// Populates `workdir/src/` from the PKGBUILD's `source=` array: remote files
/// through `fetch`, local files from `pkgbuild_dir`, `git+` URLs through the
/// host's git. Checksums are positional; `SKIP` skips one entry; git sources
/// are never summed (they have no stable byte serialization to sum).
pub fn stage(pb: &Pkgbuild, pkgbuild_dir: &Path, workdir: &Path, fetch: Fetch) -> Result<()> {
    let srcdir = workdir.join("src");
    std::fs::create_dir_all(&srcdir)?;

    let sums = if !pb.sha256sums.is_empty() {
        Sums::Sha256(&pb.sha256sums)
    } else if !pb.b2sums.is_empty() {
        Sums::B2(&pb.b2sums)
    } else {
        Sums::None
    };
    if let Sums::Sha256(s) | Sums::B2(s) = &sums {
        if s.len() != pb.source.len() {
            return Err(Error::Extraction(format!(
                "{} sources but {} checksums",
                pb.source.len(),
                s.len()
            )));
        }
    }

    for (i, entry) in pb.source.iter().enumerate() {
        let (dest, url) = match entry.split_once("::") {
            Some((d, u)) => (Some(d), u),
            None => (None, entry.as_str()),
        };
        if let Some(git_url) = url.strip_prefix("git+") {
            stage_git(git_url, dest, &srcdir)?;
        } else if url.contains("://") {
            let bytes = fetch(url)?;
            let name = dest.map(str::to_string).unwrap_or_else(|| remote_file_name(url));
            verify(&sums, i, &name, &bytes)?;
            let staged = srcdir.join(&name);
            std::fs::write(&staged, &bytes)?;
            extract_if_archive(&staged, &srcdir)?;
        } else {
            // A bare name is a file shipped beside the PKGBUILD.
            let name = dest.unwrap_or(url);
            let bytes = std::fs::read(pkgbuild_dir.join(url)).map_err(|e| {
                Error::Extraction(format!("local source {url}: {e}"))
            })?;
            verify(&sums, i, name, &bytes)?;
            let staged = srcdir.join(name);
            std::fs::write(&staged, &bytes)?;
            extract_if_archive(&staged, &srcdir)?;
        }
    }
    Ok(())
}

fn stage_git(url_with_fragment: &str, dest: Option<&str>, srcdir: &Path) -> Result<()> {
    let (url, fragment) = match url_with_fragment.split_once('#') {
        Some((u, f)) => (u, Some(f)),
        None => (url_with_fragment, None),
    };
    let name = dest.map(str::to_string).unwrap_or_else(|| {
        let base = url.rsplit('/').next().unwrap_or(url);
        base.strip_suffix(".git").unwrap_or(base).to_string()
    });
    let checkout = srcdir.join(&name);
    let ok = Command::new("git")
        .args(["clone", "--quiet", url])
        .arg(&checkout)
        .status()
        .map_err(|e| Error::Extraction(format!("spawning git: {e}")))?
        .success();
    if !ok {
        return Err(Error::Extraction(format!("git clone of {url} failed")));
    }
    if let Some(fragment) = fragment {
        // `tag=`, `commit=`, `branch=` all resolve to one rev to check out.
        let rev = fragment
            .strip_prefix("tag=")
            .or_else(|| fragment.strip_prefix("commit="))
            .or_else(|| fragment.strip_prefix("branch="))
            .ok_or_else(|| {
                Error::Extraction(format!("unsupported git fragment #{fragment} on {url}"))
            })?;
        let ok = Command::new("git")
            .current_dir(&checkout)
            .args(["checkout", "--quiet", rev])
            .status()
            .map_err(|e| Error::Extraction(format!("spawning git: {e}")))?
            .success();
        if !ok {
            return Err(Error::Extraction(format!("git checkout of {rev} in {url} failed")));
        }
    }
    Ok(())
}

fn remote_file_name(url: &str) -> String {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    no_query.rsplit('/').next().unwrap_or(no_query).to_string()
}

fn verify(sums: &Sums, i: usize, file: &str, bytes: &[u8]) -> Result<()> {
    let (expected, got) = match sums {
        Sums::Sha256(s) => (&s[i], hex::encode(sha2::Sha256::digest(bytes))),
        Sums::B2(s) => (&s[i], hex::encode(blake2::Blake2b512::digest(bytes))),
        Sums::None => return Ok(()),
    };
    if expected == "SKIP" || expected.eq_ignore_ascii_case(&got) {
        Ok(())
    } else {
        Err(Error::ChecksumMismatch {
            file: file.to_string(),
            expected: expected.clone(),
            got,
        })
    }
}

/// Extracts recognized tar archives into `dest`; anything else stays as the
/// staged file. `unpack_in` refuses entries that would escape `dest`, so a
/// hostile archive cannot write outside the build sandbox.
fn extract_if_archive(path: &Path, dest: &Path) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let file = std::fs::File::open(path)?;
    let reader: Box<dyn Read> = if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Box::new(flate2::read::GzDecoder::new(file))
    } else if name.ends_with(".tar.zst") {
        Box::new(zstd::Decoder::new(file).map_err(|e| Error::Extraction(e.to_string()))?)
    } else if name.ends_with(".tar.xz") {
        Box::new(xz2::read::XzDecoder::new(file))
    } else if name.ends_with(".tar") {
        Box::new(file)
    } else {
        return Ok(());
    };
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(|e| Error::Extraction(e.to_string()))? {
        let mut entry = entry.map_err(|e| Error::Extraction(e.to_string()))?;
        let unpacked = entry
            .unpack_in(dest)
            .map_err(|e| Error::Extraction(format!("{name}: {e}")))?;
        if !unpacked {
            let p: PathBuf = entry.path().map(|p| p.into_owned()).unwrap_or_default();
            return Err(Error::Extraction(format!(
                "{name}: entry {} would escape the source directory",
                p.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{gzipped, tar_bytes};

    fn pb(source: &[&str], sha256sums: &[&str]) -> Pkgbuild {
        Pkgbuild {
            pkgname: vec!["t".into()],
            pkgver: "1".into(),
            pkgrel: "1".into(),
            source: source.iter().map(|s| s.to_string()).collect(),
            sha256sums: sha256sums.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn sha(bytes: &[u8]) -> String {
        hex::encode(sha2::Sha256::digest(bytes))
    }

    fn no_fetch(url: &str) -> Result<Vec<u8>> {
        panic!("unexpected fetch of {url}")
    }

    #[test]
    fn a_remote_tarball_is_fetched_verified_and_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let archive = gzipped(&tar_bytes(&[("hello-1/README", "hi\n")]));
        let sum = sha(&archive);
        let pb = pb(&["https://example.com/dl/hello-1.tar.gz?ref=x"], &[&sum]);
        let fetched = archive.clone();
        stage(&pb, dir.path(), dir.path(), &move |url| {
            assert_eq!(url, "https://example.com/dl/hello-1.tar.gz?ref=x");
            Ok(fetched.clone())
        })
        .unwrap();
        // Extracted tree AND the archive itself, query string stripped.
        let readme = dir.path().join("src/hello-1/README");
        assert_eq!(std::fs::read_to_string(readme).unwrap(), "hi\n");
        assert!(dir.path().join("src/hello-1.tar.gz").exists());
    }

    #[test]
    fn a_plain_file_is_staged_but_not_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"--- a\n+++ b\n".to_vec();
        let pb = pb(&["https://example.com/fix.patch"], &[&sha(&content)]);
        let fetched = content.clone();
        stage(&pb, dir.path(), dir.path(), &move |_| Ok(fetched.clone())).unwrap();
        assert!(dir.path().join("src/fix.patch").exists());
    }

    #[test]
    fn a_local_file_is_read_from_beside_the_pkgbuild() {
        let dir = tempfile::tempdir().unwrap();
        let pkgbuild_dir = dir.path().join("pkg");
        std::fs::create_dir_all(&pkgbuild_dir).unwrap();
        std::fs::write(pkgbuild_dir.join("local.patch"), "p").unwrap();
        let present = pb(&["local.patch"], &[&sha(b"p")]);
        stage(&present, &pkgbuild_dir, dir.path(), &no_fetch).unwrap();
        assert!(dir.path().join("src/local.patch").exists());
        // Negative: a missing local file names itself.
        let absent = pb(&["absent.patch"], &["SKIP"]);
        let err = stage(&absent, &pkgbuild_dir, dir.path(), &no_fetch).unwrap_err();
        assert!(matches!(err, Error::Extraction(ref m) if m.contains("absent.patch")), "{err:?}");
    }

    #[test]
    fn a_wrong_checksum_refuses_before_anything_is_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let archive = gzipped(&tar_bytes(&[("evil-1/payload", "boom")]));
        let pb = pb(&["https://example.com/evil-1.tar.gz"], &[&"0".repeat(64)]);
        let fetched = archive.clone();
        let err = stage(&pb, dir.path(), dir.path(), &move |_| Ok(fetched.clone())).unwrap_err();
        match err {
            Error::ChecksumMismatch { file, expected, got } => {
                assert_eq!(file, "evil-1.tar.gz");
                assert_eq!(expected, "0".repeat(64));
                assert_eq!(got, sha(&archive));
            }
            other => panic!("expected ChecksumMismatch, got {other:?}"),
        }
        // The payload never touched the source tree.
        assert!(!dir.path().join("src/evil-1/payload").exists());
        assert!(!dir.path().join("src/evil-1.tar.gz").exists());
    }

    #[test]
    fn skip_skips_one_position_without_excusing_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let good = b"known".to_vec();
        let skipped = pb(
            &["https://example.com/a.bin", "https://example.com/b.bin"],
            &["SKIP", &sha(&good)],
        );
        let fetched = good.clone();
        stage(&skipped, dir.path(), dir.path(), &move |url| {
            Ok(if url.ends_with("a.bin") { b"anything at all".to_vec() } else { fetched.clone() })
        })
        .unwrap();
        // Negative: SKIP on one entry does not blind the other.
        let wrong = pb(
            &["https://example.com/a.bin", "https://example.com/b.bin"],
            &["SKIP", &"1".repeat(64)],
        );
        let err = stage(&wrong, dir.path(), dir.path(), &|_| Ok(b"x".to_vec())).unwrap_err();
        assert!(matches!(err, Error::ChecksumMismatch { .. }), "{err:?}");
    }

    #[test]
    fn checksum_count_must_match_source_count() {
        let dir = tempfile::tempdir().unwrap();
        let pb = pb(&["https://example.com/a", "https://example.com/b"], &["SKIP"]);
        let err = stage(&pb, dir.path(), dir.path(), &no_fetch).unwrap_err();
        assert!(matches!(err, Error::Extraction(ref m) if m.contains("2 sources but 1 checksums")));
    }

    #[test]
    fn b2sums_verify_through_blake2() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"blake me".to_vec();
        let b2 = hex::encode(blake2::Blake2b512::digest(&content));
        let mut pkg = pb(&["https://example.com/f.bin"], &[]);
        pkg.b2sums = vec![b2];
        let fetched = content.clone();
        stage(&pkg, dir.path(), dir.path(), &move |_| Ok(fetched.clone())).unwrap();
        // Negative: a wrong b2 refuses too.
        let mut pkg = pb(&["https://example.com/f.bin"], &[]);
        pkg.b2sums = vec!["f".repeat(128)];
        let err = stage(&pkg, dir.path(), dir.path(), &|_| Ok(b"blake me".to_vec())).unwrap_err();
        assert!(matches!(err, Error::ChecksumMismatch { .. }));
    }

    #[test]
    fn a_git_source_clones_and_checks_out_the_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            let ok = Command::new("git")
                .args([
                    "-c", "user.email=t@t", "-c", "user.name=t", "-c", "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        git(&["init", "--quiet"], &origin);
        std::fs::write(origin.join("f.txt"), "v1").unwrap();
        git(&["add", "."], &origin);
        git(&["commit", "--quiet", "-m", "one"], &origin);
        git(&["tag", "release"], &origin);
        std::fs::write(origin.join("f.txt"), "v2").unwrap();
        git(&["add", "."], &origin);
        git(&["commit", "--quiet", "-m", "two"], &origin);

        let url = format!("git+file://{}#tag=release", origin.display());
        let pb = pb(&[&format!("checkout::{url}")], &["SKIP"]);
        stage(&pb, dir.path(), dir.path(), &no_fetch).unwrap();
        // The tag's content, not the branch head's.
        let staged = dir.path().join("src/checkout/f.txt");
        assert_eq!(std::fs::read_to_string(staged).unwrap(), "v1");
    }
}
