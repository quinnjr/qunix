//! Index refresh: two official repository databases from one mirror, plus the
//! AUR metadata dump. The HTTP edge is isolated in [`http::fetch`] so the
//! ingest path — which is all of the logic — tests offline.

use crate::aur::parse_aur_dump;
use crate::error::Result;
use crate::index::{Index, Repo};
use crate::repodb::parse_repo_db;

pub struct SyncConfig {
    pub mirror: String,
    pub aur_dump_url: String,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            // Arch's geo-routed default; overridable once a config file earns
            // its keep.
            mirror: "https://geo.mirror.pkgbuild.com".into(),
            aur_dump_url: "https://aur.archlinux.org/packages-meta-ext-v1.json.gz".into(),
        }
    }
}

pub struct SyncReport {
    pub official: usize,
    pub aur: usize,
}

pub fn run(index: &Index, config: &SyncConfig, now: u64) -> Result<SyncReport> {
    // Three independent payloads, and the AUR dump dominates the wall clock:
    // fetch them concurrently so a sync costs the slowest transfer, not the
    // sum. Ingest still only runs when all three arrived, so the
    // all-or-nothing stamping semantics are unchanged.
    let core_url = format!("{}/core/os/x86_64/core.db", config.mirror);
    let extra_url = format!("{}/extra/os/x86_64/extra.db", config.mirror);
    let (core, extra, aur) = std::thread::scope(|s| {
        let extra = s.spawn(|| http::fetch(&extra_url));
        let aur = s.spawn(|| http::fetch(&config.aur_dump_url));
        let core = http::fetch(&core_url);
        // A panic in a fetch thread is a bug, not a network failure.
        (core, extra.join().expect("fetch thread panicked"), aur.join().expect("fetch thread panicked"))
    });
    ingest(index, &core?, &extra?, &aur?, now)
}

pub fn ingest(
    index: &Index,
    core: &[u8],
    extra: &[u8],
    aur: &[u8],
    now: u64,
) -> Result<SyncReport> {
    // Parse everything first, then land it in one write transaction — one
    // commit+fsync instead of two, and a mid-ingest crash can no longer
    // leave official rows committed without their AUR counterparts.
    let mut all = parse_repo_db(core, Repo::Core)?;
    all.extend(parse_repo_db(extra, Repo::Extra)?);
    let official = all.len();
    all.extend(parse_aur_dump(aur)?);
    let aur_count = all.len() - official;
    index.upsert_packages(&all)?;
    // Stamped only after the rows land: a sync that died before the commit
    // must read as "never happened", not as fresh.
    index.set_sync_time("official", now)?;
    index.set_sync_time("aur", now)?;
    Ok(SyncReport { official, aur: aur_count })
}

pub mod http {
    use std::io::Read;
    use std::time::Duration;

    use crate::error::{Error, Result};

    /// Streaming variant for the build pipeline's `sources::Fetch` contract:
    /// the body never has to exist in memory.
    pub fn fetch_reader(url: &str) -> Result<Box<dyn Read + Send>> {
        let response = ureq::get(url)
            .timeout(Duration::from_secs(60))
            .call()
            .map_err(|e| Error::Network(format!("{url}: {e}")))?;
        Ok(Box::new(response.into_reader()))
    }

    pub fn fetch(url: &str) -> Result<Vec<u8>> {
        let response = ureq::get(url)
            .timeout(Duration::from_secs(60))
            .call()
            .map_err(|e| Error::Network(format!("{url}: {e}")))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| Error::Network(format!("{url}: {e}")))?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{mini_aur, mini_db};

    #[test]
    fn ingest_populates_all_three_sources_and_stamps_both_clocks() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("i.redb")).unwrap();
        let report = ingest(
            &index,
            &mini_db("linux-api-headers", "6.10-1"),
            &mini_db("zsh", "5.9-5"),
            &mini_aur(&[("zsh-git", "5.9.r380-1")]),
            1_000,
        )
        .unwrap();
        assert_eq!((report.official, report.aur), (2, 1));
        assert_eq!(index.get("zsh").unwrap().unwrap().repo, Repo::Extra);
        assert_eq!(index.get("zsh-git").unwrap().unwrap().repo, Repo::Aur);
        assert_eq!(index.sync_time("official").unwrap(), Some(1_000));
        assert_eq!(index.sync_time("aur").unwrap(), Some(1_000));
    }

    #[test]
    fn reingest_upserts_versions_without_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("i.redb")).unwrap();
        let aur = mini_aur(&[]);
        ingest(&index, &mini_db("zsh", "5.9-4"), &mini_db("x", "1-1"), &aur, 1_000).unwrap();
        ingest(&index, &mini_db("zsh", "5.9-5"), &mini_db("x", "1-1"), &aur, 2_000).unwrap();
        assert_eq!(index.get("zsh").unwrap().unwrap().version, "5.9-5");
        // Negative: replaced, not accumulated.
        assert_eq!(index.search("zsh").unwrap().len(), 1);
    }

    #[test]
    fn a_failed_ingest_does_not_stamp_the_clocks() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("i.redb")).unwrap();
        let err = ingest(&index, b"garbage", &mini_db("x", "1-1"), &mini_aur(&[]), 1_000);
        assert!(err.is_err());
        // The half-sync reads as "never synced", not as fresh.
        assert_eq!(index.sync_time("official").unwrap(), None);
        assert_eq!(index.sync_time("aur").unwrap(), None);
    }

    #[cfg(feature = "online")]
    #[test]
    fn live_sync_lands_a_real_index() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("i.redb")).unwrap();
        let report = run(&index, &SyncConfig::default(), 1).unwrap();
        assert!(report.official > 1_000, "core+extra is thousands of packages");
        assert!(report.aur > 10_000, "the AUR is tens of thousands");
        assert!(index.get("zsh").unwrap().is_some());
    }
}
