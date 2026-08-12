//! Parser for pacman repository databases: a compressed tar whose entries are
//! `<pkgname>-<pkgver>/desc` files of `%FIELD%` blocks. The format is a fact
//! about pacman's on-disk layout that any compatible reader must match.

use std::io::Read;

use crate::error::{Error, Result};
use crate::index::{PackageRecord, Repo};

/// Mirrors serve `.db` as gzip or zstd depending on repo configuration, and
/// the file name says nothing — so the *magic bytes* choose the decoder.
pub fn parse_repo_db(mut reader: impl Read, repo: Repo) -> Result<Vec<PackageRecord>> {
    let mut raw = Vec::new();
    reader.read_to_end(&mut raw)?;
    let decoded: Box<dyn Read> = match raw[..] {
        [0x1f, 0x8b, ..] => Box::new(flate2::read::GzDecoder::new(raw.as_slice())),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Box::new(
            zstd::Decoder::new(raw.as_slice()).map_err(|e| Error::Extraction(e.to_string()))?,
        ),
        _ => {
            return Err(Error::Extraction(
                "repository database is neither gzip nor zstd".into(),
            ));
        }
    };
    let mut archive = tar::Archive::new(decoded);
    let mut out = Vec::new();
    for entry in archive.entries().map_err(|e| Error::Extraction(e.to_string()))? {
        let mut entry = entry.map_err(|e| Error::Extraction(e.to_string()))?;
        let is_desc = entry
            .path()
            .ok()
            .is_some_and(|p| p.file_name().is_some_and(|f| f == "desc"));
        if !is_desc {
            continue;
        }
        let mut text = String::new();
        entry.read_to_string(&mut text).map_err(|e| Error::Extraction(e.to_string()))?;
        if let Some(rec) = parse_desc(&text, repo) {
            out.push(rec);
        }
    }
    Ok(out)
}

/// One desc file: `%FIELD%` header lines, value lines beneath, blank-line
/// terminated. A desc without both `%NAME%` and `%VERSION%` is skipped — one
/// malformed entry must not sink the other few thousand.
fn parse_desc(text: &str, repo: Repo) -> Option<PackageRecord> {
    let mut name = None;
    let mut base = None;
    let mut version = None;
    let mut description = String::new();
    let mut url = String::new();
    let mut depends = Vec::new();
    let mut makedepends = Vec::new();

    let mut field: Option<&str> = None;
    for line in text.lines() {
        if line.is_empty() {
            field = None;
            continue;
        }
        if line.starts_with('%') && line.ends_with('%') && line.len() > 2 {
            field = Some(&line[1..line.len() - 1]);
            continue;
        }
        match field {
            Some("NAME") => name = Some(line.to_string()),
            Some("BASE") => base = Some(line.to_string()),
            Some("VERSION") => version = Some(line.to_string()),
            Some("DESC") => description = line.to_string(),
            Some("URL") => url = line.to_string(),
            Some("DEPENDS") => depends.push(line.to_string()),
            Some("MAKEDEPENDS") => makedepends.push(line.to_string()),
            _ => {}
        }
    }
    let name = name?;
    Some(PackageRecord {
        // A desc without %BASE% is a non-split package: its base is itself.
        package_base: base.unwrap_or_else(|| name.clone()),
        name,
        version: version?,
        repo,
        description,
        url,
        depends,
        makedepends,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{desc, gzipped, tar_bytes, zstded};

    fn sample_tar() -> Vec<u8> {
        let zsh = desc(&[
            ("NAME", &["zsh"]),
            ("BASE", &["zsh"]),
            ("VERSION", &["5.9-5"]),
            ("DESC", &["A very advanced and programmable command interpreter"]),
            ("URL", &["https://www.zsh.org/"]),
            ("DEPENDS", &["pcre2", "libcap", "ncurses"]),
            ("MAKEDEPENDS", &["yodl"]),
        ]);
        let minimal = desc(&[("NAME", &["tiny"]), ("VERSION", &["1-1"])]);
        let nameless = desc(&[("VERSION", &["9-9"]), ("DESC", &["broken entry"])]);
        tar_bytes(&[
            ("zsh-5.9-5/desc", &zsh),
            // Non-desc entries exist in real databases and must be ignored.
            ("zsh-5.9-5/files", "%FILES%\nusr/bin/zsh\n"),
            ("tiny-1-1/desc", &minimal),
            ("broken-9-9/desc", &nameless),
        ])
    }

    fn assert_sample(records: &[PackageRecord]) {
        // The nameless desc is skipped; the rest survive it.
        assert_eq!(records.len(), 2);
        let zsh = records.iter().find(|r| r.name == "zsh").unwrap();
        assert_eq!(zsh.version, "5.9-5");
        assert_eq!(zsh.depends, ["pcre2", "libcap", "ncurses"]);
        assert_eq!(zsh.makedepends, ["yodl"]);
        assert_eq!(zsh.url, "https://www.zsh.org/");
        let tiny = records.iter().find(|r| r.name == "tiny").unwrap();
        // No %BASE% means the package is its own base.
        assert_eq!(tiny.package_base, "tiny");
        assert_eq!(tiny.description, "");
        assert!(tiny.depends.is_empty());
        // Negative: nothing invented a record from the `files` entry.
        assert!(!records.iter().any(|r| r.description.contains("%FILES%")));
    }

    #[test]
    fn parses_a_gzip_database() {
        let records = parse_repo_db(gzipped(&sample_tar()).as_slice(), Repo::Extra).unwrap();
        assert_sample(&records);
        assert!(records.iter().all(|r| r.repo == Repo::Extra));
    }

    #[test]
    fn parses_the_same_database_under_zstd() {
        // Same archive, other compressor: proves the choice is magic-byte
        // sniffing, not anything about the caller or a file name.
        let records = parse_repo_db(zstded(&sample_tar()).as_slice(), Repo::Core).unwrap();
        assert_sample(&records);
        assert!(records.iter().all(|r| r.repo == Repo::Core));
    }

    #[test]
    fn refuses_unrecognized_compression() {
        let err = parse_repo_db(&b"plain tar or garbage"[..], Repo::Core).unwrap_err();
        assert!(matches!(err, Error::Extraction(_)), "got {err:?}");
    }
}
