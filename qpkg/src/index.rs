//! The on-disk index: one redb file, three tables. Values are serde_json
//! bytes — the dataset is ~100k rows read by a CLI, so decode speed is not
//! the constraint; schema tolerance is, and JSON plus `#[serde(default)]`
//! lets an older record decode under a newer field set.

use std::fmt;
use std::path::{Path, PathBuf};

use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const PACKAGES: TableDefinition<&str, &[u8]> = TableDefinition::new("packages");
const BUILT: TableDefinition<&str, &[u8]> = TableDefinition::new("built");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repo {
    Core,
    Extra,
    Aur,
}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Repo::Core => "core",
            Repo::Extra => "extra",
            Repo::Aur => "aur",
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PackageRecord {
    pub name: String,
    pub version: String,
    pub repo: Repo,
    /// The pkgbase this package's PKGBUILD lives under — differs from `name`
    /// for split packages, and PKGBUILDs are only fetchable by base.
    #[serde(default)]
    pub package_base: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(default)]
    pub makedepends: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BuiltRecord {
    pub name: String,
    pub version_built: String,
    pub pkgbuild_sha256: String,
    pub artifact_path: PathBuf,
    pub built_at_unix: u64,
}

pub struct Index {
    db: Database,
}

/// Every redb error collapses into `Error::Index`: the caller's options are
/// identical (re-sync or delete the file), so the distinctions redb draws are
/// not worth surfacing past the message text.
fn idx(e: impl fmt::Display) -> Error {
    Error::Index(e.to_string())
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)
            .map_err(|e| Error::Index(format!("{}: {e}", path.display())))?;
        // Open every table once so a fresh database serves reads without a
        // "table missing" special case.
        let tx = db.begin_write().map_err(idx)?;
        tx.open_table(PACKAGES).map_err(idx)?;
        tx.open_table(BUILT).map_err(idx)?;
        tx.open_table(META).map_err(idx)?;
        tx.commit().map_err(idx)?;
        Ok(Self { db })
    }

    pub fn upsert_packages(&self, records: &[PackageRecord]) -> Result<()> {
        let tx = self.db.begin_write().map_err(idx)?;
        {
            let mut table = tx.open_table(PACKAGES).map_err(idx)?;
            for rec in records {
                let bytes = serde_json::to_vec(rec).map_err(idx)?;
                table.insert(rec.name.as_str(), bytes.as_slice()).map_err(idx)?;
            }
        }
        tx.commit().map_err(idx)?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<Option<PackageRecord>> {
        let tx = self.db.begin_read().map_err(idx)?;
        let table = tx.open_table(PACKAGES).map_err(idx)?;
        match table.get(name).map_err(idx)? {
            Some(v) => Ok(Some(decode(v.value())?)),
            None => Ok(None),
        }
    }

    /// Substring search over name and description, case-insensitive. An exact
    /// name match sorts first, then name substrings, then description-only
    /// hits; ties stay in name order because the table iterates sorted.
    pub fn search(&self, term: &str) -> Result<Vec<PackageRecord>> {
        let needle = term.to_lowercase();
        let tx = self.db.begin_read().map_err(idx)?;
        let table = tx.open_table(PACKAGES).map_err(idx)?;
        let mut hits: Vec<(u8, PackageRecord)> = Vec::new();
        for entry in table.iter().map_err(idx)? {
            let (_, v) = entry.map_err(idx)?;
            let rec: PackageRecord = decode(v.value())?;
            let name = rec.name.to_lowercase();
            let rank = if name == needle {
                0
            } else if name.contains(&needle) {
                1
            } else if rec.description.to_lowercase().contains(&needle) {
                2
            } else {
                continue;
            };
            hits.push((rank, rec));
        }
        hits.sort_by_key(|(rank, _)| *rank);
        Ok(hits.into_iter().map(|(_, rec)| rec).collect())
    }

    pub fn record_built(&self, rec: &BuiltRecord) -> Result<()> {
        let tx = self.db.begin_write().map_err(idx)?;
        {
            let mut table = tx.open_table(BUILT).map_err(idx)?;
            let bytes = serde_json::to_vec(rec).map_err(idx)?;
            table.insert(rec.name.as_str(), bytes.as_slice()).map_err(idx)?;
        }
        tx.commit().map_err(idx)?;
        Ok(())
    }

    pub fn built(&self, name: &str) -> Result<Option<BuiltRecord>> {
        let tx = self.db.begin_read().map_err(idx)?;
        let table = tx.open_table(BUILT).map_err(idx)?;
        match table.get(name).map_err(idx)? {
            Some(v) => Ok(Some(decode(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn all_built(&self) -> Result<Vec<BuiltRecord>> {
        let tx = self.db.begin_read().map_err(idx)?;
        let table = tx.open_table(BUILT).map_err(idx)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(idx)? {
            let (_, v) = entry.map_err(idx)?;
            out.push(decode(v.value())?);
        }
        Ok(out)
    }

    pub fn set_sync_time(&self, source: &str, unix: u64) -> Result<()> {
        let tx = self.db.begin_write().map_err(idx)?;
        {
            let mut table = tx.open_table(META).map_err(idx)?;
            let key = format!("sync:{source}");
            table.insert(key.as_str(), unix.to_le_bytes().as_slice()).map_err(idx)?;
        }
        tx.commit().map_err(idx)?;
        Ok(())
    }

    pub fn sync_time(&self, source: &str) -> Result<Option<u64>> {
        let tx = self.db.begin_read().map_err(idx)?;
        let table = tx.open_table(META).map_err(idx)?;
        let key = format!("sync:{source}");
        match table.get(key.as_str()).map_err(idx)? {
            Some(v) => {
                let bytes: [u8; 8] = v
                    .value()
                    .try_into()
                    .map_err(|_| Error::Index(format!("sync stamp for {source} is not a u64")))?;
                Ok(Some(u64::from_le_bytes(bytes)))
            }
            None => Ok(None),
        }
    }

    /// Stale means *any* source has never synced or synced longer ago than
    /// the threshold — a fresh official mirror does not excuse a missing AUR
    /// dump, because search silently spans both.
    pub fn stale(&self, sources: &[&str], now: u64, threshold_secs: u64) -> Result<bool> {
        for source in sources {
            match self.sync_time(source)? {
                Some(at) if now.saturating_sub(at) <= threshold_secs => {}
                _ => return Ok(true),
            }
        }
        Ok(false)
    }
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, version: &str, repo: Repo, description: &str) -> PackageRecord {
        PackageRecord {
            name: name.into(),
            package_base: name.into(),
            version: version.into(),
            repo,
            description: description.into(),
            url: String::new(),
            depends: vec![],
            makedepends: vec![],
        }
    }

    fn temp_index() -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("index.redb")).unwrap();
        (dir, index)
    }

    #[test]
    fn upsert_replaces_and_get_roundtrips() {
        let (_dir, index) = temp_index();
        index.upsert_packages(&[record("zsh", "5.9-1", Repo::Extra, "the Z shell")]).unwrap();
        index.upsert_packages(&[record("zsh", "5.9-2", Repo::Extra, "the Z shell")]).unwrap();
        let got = index.get("zsh").unwrap().unwrap();
        assert_eq!(got.version, "5.9-2");
        // Negative: replaced, not duplicated — and absent names are None.
        assert_eq!(index.search("zsh").unwrap().len(), 1);
        assert!(index.get("does-not-exist").unwrap().is_none());
    }

    #[test]
    fn search_ranks_exact_name_then_name_then_description() {
        let (_dir, index) = temp_index();
        index
            .upsert_packages(&[
                record("grml-zsh-config", "1-1", Repo::Extra, "config"),
                record("zsh", "5.9-1", Repo::Extra, "the Z shell"),
                record("fish", "4.0-1", Repo::Extra, "shell with zsh-like features"),
                record("unrelated", "1-1", Repo::Core, "nothing here"),
            ])
            .unwrap();
        let hits = index.search("zsh").unwrap();
        let names: Vec<&str> = hits.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["zsh", "grml-zsh-config", "fish"]);
        // Negative: non-matching rows are absent, not ranked last.
        assert!(!names.contains(&"unrelated"));
        // Case-insensitive, description path.
        assert_eq!(index.search("Z SHELL").unwrap()[0].name, "zsh");
    }

    #[test]
    fn built_records_roundtrip() {
        let (_dir, index) = temp_index();
        assert!(index.built("zsh").unwrap().is_none());
        let rec = BuiltRecord {
            name: "zsh".into(),
            version_built: "5.9-1".into(),
            pkgbuild_sha256: "ab".repeat(32),
            artifact_path: PathBuf::from("/tmp/zsh-5.9-1-x86_64.pkg.tar.zst"),
            built_at_unix: 1_700_000_000,
        };
        index.record_built(&rec).unwrap();
        assert_eq!(index.built("zsh").unwrap().unwrap(), rec);
        assert_eq!(index.all_built().unwrap(), vec![rec]);
    }

    #[test]
    fn staleness_needs_every_source_fresh() {
        let (_dir, index) = temp_index();
        let now = 1_000_000;
        // Negative first: nothing synced is stale.
        assert!(index.stale(&["official", "aur"], now, 3600).unwrap());
        index.set_sync_time("official", now - 100).unwrap();
        // One fresh source does not cover a missing one.
        assert!(index.stale(&["official", "aur"], now, 3600).unwrap());
        index.set_sync_time("aur", now - 100).unwrap();
        assert!(!index.stale(&["official", "aur"], now, 3600).unwrap());
        // Ageing past the threshold flips it back.
        assert!(index.stale(&["official", "aur"], now + 4000, 3600).unwrap());
    }

    #[test]
    fn corrupt_database_reports_rather_than_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.redb");
        std::fs::write(&path, b"this is not a redb file, not even close").unwrap();
        let err = Index::open(&path).err().expect("opening garbage must fail");
        match err {
            Error::Index(msg) => assert!(msg.contains("index.redb"), "message names the file: {msg}"),
            other => panic!("expected Error::Index, got {other:?}"),
        }
    }

    #[test]
    fn an_older_record_decodes_under_the_newer_schema() {
        // A record written before `url`/`depends` existed must still decode —
        // serde defaults, not a migration.
        let old = br#"{"name":"zsh","version":"5.9-1","repo":"Extra","description":"shell"}"#;
        let rec: PackageRecord = decode(old).unwrap();
        assert_eq!(rec.url, "");
        assert!(rec.depends.is_empty());
        // Negative: garbage still refuses.
        assert!(decode::<PackageRecord>(b"{not json").is_err());
    }
}
