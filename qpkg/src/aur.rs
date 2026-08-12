//! Parser for the AUR's full metadata dump (`packages-meta-ext-v1.json.gz`):
//! one gzipped JSON array covering every AUR package, which is what makes
//! offline search possible instead of per-query RPC calls.

use std::io::Read;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::index::{PackageRecord, Repo};

/// The dump's field casing is the AUR RPC's, not ours; nullable and optional
/// fields collapse to empty here because the index treats "absent" and
/// "empty" identically.
#[derive(Deserialize)]
struct DumpEntry {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Version")]
    version: String,
    #[serde(rename = "PackageBase", default)]
    package_base: Option<String>,
    #[serde(rename = "Description", default)]
    description: Option<String>,
    #[serde(rename = "URL", default)]
    url: Option<String>,
    #[serde(rename = "Depends", default)]
    depends: Option<Vec<String>>,
    #[serde(rename = "MakeDepends", default)]
    makedepends: Option<Vec<String>>,
}

pub fn parse_aur_dump(reader: impl Read) -> Result<Vec<PackageRecord>> {
    let gz = flate2::read::GzDecoder::new(reader);
    let entries: Vec<DumpEntry> = serde_json::from_reader(gz)
        .map_err(|e| Error::Index(format!("AUR metadata dump: {e}")))?;
    Ok(entries
        .into_iter()
        .map(|e| PackageRecord {
            package_base: e.package_base.unwrap_or_else(|| e.name.clone()),
            name: e.name,
            version: e.version,
            repo: Repo::Aur,
            description: e.description.unwrap_or_default(),
            url: e.url.unwrap_or_default(),
            depends: e.depends.unwrap_or_default(),
            makedepends: e.makedepends.unwrap_or_default(),
        })
        .collect())
}

/// PKGBUILD snapshots are keyed by package *base*: a split package's members
/// share one snapshot.
pub fn snapshot_url(base: &str) -> String {
    format!("https://aur.archlinux.org/cgit/aur.git/snapshot/{base}.tar.gz")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gz(text: &str) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(text.as_bytes()).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn parses_full_null_and_missing_shapes() {
        let dump = r#"[
            {"Name":"zsh-git","PackageBase":"zsh-git","Version":"5.9.r380-1",
             "Description":"development version of zsh","URL":"https://www.zsh.org",
             "Depends":["ncurses","pcre2"],"MakeDepends":["git"]},
            {"Name":"nulls","PackageBase":"nulls","Version":"1-1",
             "Description":null,"URL":null},
            {"Name":"bare-split-child","PackageBase":"bare","Version":"2-1"}
        ]"#;
        let records = parse_aur_dump(gz(dump).as_slice()).unwrap();
        assert_eq!(records.len(), 3);

        let full = &records[0];
        assert_eq!(full.name, "zsh-git");
        assert_eq!(full.version, "5.9.r380-1");
        assert_eq!(full.depends, ["ncurses", "pcre2"]);
        assert_eq!(full.makedepends, ["git"]);
        assert!(records.iter().all(|r| r.repo == Repo::Aur));

        // Nulls become empty, not errors.
        let nulls = &records[1];
        assert_eq!(nulls.description, "");
        assert_eq!(nulls.url, "");
        assert!(nulls.depends.is_empty());

        // A split child keeps its own name but its base's snapshot.
        let child = &records[2];
        assert_eq!(child.name, "bare-split-child");
        assert_eq!(child.package_base, "bare");
        assert_eq!(
            snapshot_url(&child.package_base),
            "https://aur.archlinux.org/cgit/aur.git/snapshot/bare.tar.gz"
        );
    }

    #[test]
    fn malformed_json_reports_rather_than_panics() {
        let err = parse_aur_dump(gz("[{\"Name\":").as_slice()).unwrap_err();
        match err {
            Error::Index(msg) => assert!(msg.contains("AUR"), "context names the source: {msg}"),
            other => panic!("expected Error::Index, got {other:?}"),
        }
    }
}
