use std::env;
use std::path::PathBuf;

/// Index location: `$QPKG_DB` wins, then `$XDG_DATA_HOME/qpkg/index.redb`,
/// then `~/.local/share/qpkg/index.redb`. An *empty* override is ignored —
/// `QPKG_DB= qpkg …` almost always means "unset", not "the current directory".
pub fn db_path() -> PathBuf {
    if let Some(p) = env::var_os("QPKG_DB") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    data_dir().join("index.redb")
}

pub fn artifacts_dir() -> PathBuf {
    data_dir().join("artifacts")
}

pub fn build_dir(name: &str) -> PathBuf {
    let base = match env::var_os("XDG_CACHE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join(".cache"),
    };
    base.join("qpkg").join("build").join(name)
}

fn data_dir() -> PathBuf {
    let base = match env::var_os("XDG_DATA_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join(".local").join("share"),
    };
    base.join("qpkg")
}

fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_else(|| ".".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env vars are process-global, so these tests would race each other under
    // the parallel test runner; one test owns every case instead.
    #[test]
    fn db_path_resolution_order() {
        unsafe {
            env::set_var("HOME", "/nonexistent-home");
            env::remove_var("XDG_DATA_HOME");

            env::set_var("QPKG_DB", "/tmp/override.redb");
            assert_eq!(db_path(), PathBuf::from("/tmp/override.redb"));

            // Negative: an empty override is ignored, not honoured.
            env::set_var("QPKG_DB", "");
            assert_eq!(
                db_path(),
                PathBuf::from("/nonexistent-home/.local/share/qpkg/index.redb")
            );

            env::remove_var("QPKG_DB");
            env::set_var("XDG_DATA_HOME", "/xdg-data");
            assert_eq!(db_path(), PathBuf::from("/xdg-data/qpkg/index.redb"));
            assert_eq!(artifacts_dir(), PathBuf::from("/xdg-data/qpkg/artifacts"));

            env::remove_var("XDG_DATA_HOME");
        }
    }
}
