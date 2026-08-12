//! The read-side commands, printing to injected writers so tests capture
//! both the output and the staleness warning.

use std::io::Write;

use crate::error::{Error, Result};
use crate::index::Index;

pub const STALE_THRESHOLD_SECS: u64 = 24 * 60 * 60;
pub const SOURCES: &[&str] = &["official", "aur"];

/// A stale index warns and continues: offline search on old data beats a
/// hard network dependency in every read path.
pub fn warn_if_stale(index: &Index, warn: &mut impl Write, now: u64) -> Result<()> {
    if index.stale(SOURCES, now, STALE_THRESHOLD_SECS)? {
        writeln!(warn, "warning: the index is stale or incomplete; run `qpkg sync`")?;
    }
    Ok(())
}

pub fn search(
    index: &Index,
    term: &str,
    out: &mut impl Write,
    warn: &mut impl Write,
    now: u64,
) -> Result<()> {
    warn_if_stale(index, warn, now)?;
    for rec in index.search(term)? {
        let built = match index.built(&rec.name)? {
            Some(b) => format!("  [built {}]", b.version_built),
            None => String::new(),
        };
        writeln!(out, "{}/{} {}{}", rec.repo, rec.name, rec.version, built)?;
        if !rec.description.is_empty() {
            writeln!(out, "    {}", rec.description)?;
        }
    }
    Ok(())
}

pub fn info(
    index: &Index,
    name: &str,
    out: &mut impl Write,
    warn: &mut impl Write,
    now: u64,
) -> Result<()> {
    warn_if_stale(index, warn, now)?;
    let rec = index
        .get(name)?
        .ok_or_else(|| Error::Index(format!("{name} is not in the index (try `qpkg sync`)")))?;
    writeln!(out, "name        : {}", rec.name)?;
    writeln!(out, "base        : {}", rec.package_base)?;
    writeln!(out, "version     : {}", rec.version)?;
    writeln!(out, "repo        : {}", rec.repo)?;
    writeln!(out, "description : {}", rec.description)?;
    writeln!(out, "url         : {}", rec.url)?;
    writeln!(out, "depends     : {}", rec.depends.join(" "))?;
    writeln!(out, "makedepends : {}", rec.makedepends.join(" "))?;
    match index.built(&rec.name)? {
        Some(b) => writeln!(out, "built       : {} ({})", b.version_built, b.artifact_path.display())?,
        None => writeln!(out, "built       : never")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{BuiltRecord, PackageRecord, Repo};
    use std::path::PathBuf;

    fn seeded() -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("i.redb")).unwrap();
        index
            .upsert_packages(&[PackageRecord {
                name: "zsh".into(),
                package_base: "zsh".into(),
                version: "5.9-5".into(),
                repo: Repo::Extra,
                description: "the Z shell".into(),
                url: "https://www.zsh.org/".into(),
                depends: vec!["ncurses".into(), "pcre2".into()],
                makedepends: vec![],
            }])
            .unwrap();
        index.set_sync_time("official", 1_000).unwrap();
        index.set_sync_time("aur", 1_000).unwrap();
        (dir, index)
    }

    #[test]
    fn search_tags_repo_and_built_status() {
        let (_dir, index) = seeded();
        let mut out = Vec::new();
        let mut warn = Vec::new();
        search(&index, "zsh", &mut out, &mut warn, 2_000).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("extra/zsh 5.9-5"), "{text}");
        // Negative: never built, so no built marker and no warning.
        assert!(!text.contains("[built"));
        assert!(warn.is_empty(), "{}", String::from_utf8_lossy(&warn));

        index
            .record_built(&BuiltRecord {
                name: "zsh".into(),
                version_built: "5.9-4".into(),
                pkgbuild_sha256: String::new(),
                artifact_path: PathBuf::from("/x"),
                built_at_unix: 1,
            })
            .unwrap();
        let mut out = Vec::new();
        search(&index, "zsh", &mut out, &mut warn, 2_000).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("[built 5.9-4]"));
    }

    #[test]
    fn a_stale_index_warns_but_still_answers() {
        let (_dir, index) = seeded();
        let mut out = Vec::new();
        let mut warn = Vec::new();
        let later = 1_000 + STALE_THRESHOLD_SECS + 1;
        search(&index, "zsh", &mut out, &mut warn, later).unwrap();
        assert!(String::from_utf8(warn).unwrap().contains("stale"));
        // The answer still arrived.
        assert!(String::from_utf8(out).unwrap().contains("zsh"));
    }

    #[test]
    fn info_prints_the_record_and_misses_are_errors() {
        let (_dir, index) = seeded();
        let mut out = Vec::new();
        let mut warn = Vec::new();
        info(&index, "zsh", &mut out, &mut warn, 2_000).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("depends     : ncurses pcre2"), "{text}");
        assert!(text.contains("built       : never"));

        // Negative: an unknown name is an error naming the fix, not empty
        // output — main turns this into a nonzero exit.
        let mut out = Vec::new();
        let err = info(&index, "nope", &mut out, &mut warn, 2_000).unwrap_err();
        match err {
            Error::Index(msg) => assert!(msg.contains("qpkg sync"), "{msg}"),
            other => panic!("expected Error::Index, got {other:?}"),
        }
    }
}
