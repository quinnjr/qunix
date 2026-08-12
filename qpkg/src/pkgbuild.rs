//! PKGBUILD metadata extraction. A PKGBUILD is a bash script, so bash — real
//! bash, spawned with a scrubbed environment — evaluates it; Rust only parses
//! the `declare -p` transcript. qpkg never reimplements bash.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

/// The variables extraction asks bash to dump. Anything else a PKGBUILD
/// declares is its own business.
const VARS: &[&str] = &[
    "pkgname", "pkgver", "pkgrel", "epoch", "arch", "source", "sha256sums", "b2sums", "depends",
    "makedepends", "options",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pkgbuild {
    pub pkgname: Vec<String>,
    pub pkgver: String,
    pub pkgrel: String,
    pub epoch: Option<String>,
    pub arch: Vec<String>,
    pub source: Vec<String>,
    pub sha256sums: Vec<String>,
    pub b2sums: Vec<String>,
    pub depends: Vec<String>,
    pub makedepends: Vec<String>,
    pub options: Vec<String>,
    /// Every shell function the PKGBUILD defines — `build`, `package`,
    /// `pkgver`, `package_<name>` splits, helpers, all of them.
    pub functions: BTreeSet<String>,
}

impl Pkgbuild {
    pub fn full_version(&self) -> String {
        match &self.epoch {
            Some(e) => format!("{e}:{}-{}", self.pkgver, self.pkgrel),
            None => format!("{}-{}", self.pkgver, self.pkgrel),
        }
    }

    pub fn check_arch(&self) -> Result<()> {
        if self.arch.iter().any(|a| a == "x86_64" || a == "any") {
            Ok(())
        } else {
            Err(Error::UnsupportedArch {
                name: self.pkgname.first().cloned().unwrap_or_default(),
                arches: self.arch.clone(),
            })
        }
    }
}

/// Sources the PKGBUILD in `bash --noprofile --norc` with an environment
/// scrubbed down to `PATH`, `LC_ALL=C` and a `HOME` pointed at `home` — the
/// evaluation must see the build sandbox, never the caller's account.
pub fn extract(path: &Path, home: &Path) -> Result<Pkgbuild> {
    let script = format!(
        "set -e\nsource \"$1\"\nfor v in {vars}; do declare -p \"$v\" 2>/dev/null || true; done\ndeclare -F",
        vars = VARS.join(" ")
    );
    let output = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", &script, "qpkg-extract"])
        .arg(path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("HOME", home)
        .output()
        .map_err(|e| Error::Extraction(format!("spawning bash: {e}")))?;
    if !output.status.success() {
        return Err(Error::Extraction(format!(
            "sourcing {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_transcript(&text)
}

enum Value {
    Scalar(String),
    Array(Vec<String>),
}

fn parse_transcript(text: &str) -> Result<Pkgbuild> {
    let mut vars: BTreeMap<String, Value> = BTreeMap::new();
    let mut functions = BTreeSet::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("declare ") else {
            // `declare -p` of a multi-line value continues on following
            // lines; those only occur inside $'…' quoting, which is rejected
            // below when the opening line is seen.
            continue;
        };
        let (flags, body) = rest.split_once(' ').unwrap_or((rest, ""));
        if flags == "-f" {
            // From `declare -F`: one function name per line.
            functions.insert(body.trim().to_string());
            continue;
        }
        let Some((name, raw)) = body.split_once('=') else {
            continue;
        };
        let value = if flags.contains('a') || flags.contains('A') {
            Value::Array(parse_array(name, raw)?)
        } else {
            Value::Scalar(parse_scalar(name, raw)?)
        };
        vars.insert(name.to_string(), value);
    }

    let scalar = |vars: &BTreeMap<String, Value>, name: &str| -> Option<String> {
        match vars.get(name) {
            Some(Value::Scalar(s)) => Some(s.clone()),
            // An array where a scalar was expected: first element wins,
            // matching how bash itself expands `$var`.
            Some(Value::Array(a)) => a.first().cloned(),
            None => None,
        }
    };
    let array = |vars: &BTreeMap<String, Value>, name: &str| -> Vec<String> {
        match vars.get(name) {
            Some(Value::Array(a)) => a.clone(),
            // pkgname is frequently a bare scalar; every array field accepts
            // the one-element spelling.
            Some(Value::Scalar(s)) => vec![s.clone()],
            None => Vec::new(),
        }
    };

    let pkgname = array(&vars, "pkgname");
    if pkgname.is_empty() {
        return Err(Error::Extraction("PKGBUILD declares no pkgname".into()));
    }
    Ok(Pkgbuild {
        pkgname,
        pkgver: scalar(&vars, "pkgver")
            .ok_or_else(|| Error::Extraction("PKGBUILD declares no pkgver".into()))?,
        pkgrel: scalar(&vars, "pkgrel").unwrap_or_else(|| "1".into()),
        epoch: scalar(&vars, "epoch"),
        arch: array(&vars, "arch"),
        source: array(&vars, "source"),
        sha256sums: array(&vars, "sha256sums"),
        b2sums: array(&vars, "b2sums"),
        depends: array(&vars, "depends"),
        makedepends: array(&vars, "makedepends"),
        options: array(&vars, "options"),
        functions,
    })
}

/// `"value"` with `\`-escapes, or a bare word (`declare -i n=5`). `$'…'`
/// quoting means the value holds control characters no PKGBUILD field should
/// — refused by name rather than mis-decoded.
fn parse_scalar(name: &str, raw: &str) -> Result<String> {
    if let Some(quoted) = raw.strip_prefix('"') {
        unescape(name, quoted)
    } else if raw.starts_with("$'") {
        Err(Error::Extraction(format!("{name} uses \\$'…' quoting, which qpkg refuses")))
    } else {
        Ok(raw.to_string())
    }
}

/// `([0]="a" [1]="b")` — indices are trusted for order of appearance, which
/// is how `declare -p` emits them.
fn parse_array(name: &str, raw: &str) -> Result<Vec<String>> {
    let inner = raw
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| Error::Extraction(format!("{name} is not a bash array literal")))?;
    let mut out = Vec::new();
    let mut rest = inner;
    while let Some(open) = rest.find('[') {
        let close = rest[open..]
            .find("]=")
            .ok_or_else(|| Error::Extraction(format!("{name}: malformed array element")))?;
        let after = &rest[open + close + 2..];
        if after.starts_with("$'") {
            return Err(Error::Extraction(format!("{name} uses \\$'…' quoting, which qpkg refuses")));
        }
        let quoted = after
            .strip_prefix('"')
            .ok_or_else(|| Error::Extraction(format!("{name}: unquoted array element")))?;
        let (value, consumed) = take_quoted(name, quoted)?;
        out.push(value);
        rest = &quoted[consumed..];
    }
    Ok(out)
}

/// Reads up to the closing unescaped `"`, returning the unescaped value and
/// how many bytes of `s` were consumed (closing quote included).
fn take_quoted(name: &str, s: &str) -> Result<(String, usize)> {
    let mut value = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some((_, next)) => value.push(next),
                None => break,
            },
            '"' => return Ok((value, i + 1)),
            _ => value.push(c),
        }
    }
    Err(Error::Extraction(format!("{name}: unterminated quoted value")))
}

fn unescape(name: &str, quoted_with_close: &str) -> Result<String> {
    let (value, consumed) = take_quoted(name, quoted_with_close)?;
    // A scalar line carries nothing after its closing quote.
    if !quoted_with_close[consumed..].is_empty() {
        return Err(Error::Extraction(format!("{name}: trailing content after closing quote")));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write_pkgbuild(dir: &tempfile::TempDir, content: &str) -> std::path::PathBuf {
        let path = dir.path().join("PKGBUILD");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn simple_scalars_extract_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pkgbuild(
            &dir,
            "pkgname=hello\npkgver=2.12\npkgrel=3\narch=(x86_64)\nsource=(\"https://example.com/hello-$pkgver.tar.gz\")\nsha256sums=('deadbeef')\n",
        );
        let pb = extract(&path, dir.path()).unwrap();
        assert_eq!(pb.pkgname, ["hello"]);
        assert_eq!(pb.pkgver, "2.12");
        assert_eq!(pb.pkgrel, "3");
        assert_eq!(pb.epoch, None);
        // $pkgver interpolated by bash, not by us.
        assert_eq!(pb.source, ["https://example.com/hello-2.12.tar.gz"]);
        assert_eq!(pb.full_version(), "2.12-3");
        assert!(pb.check_arch().is_ok());
        assert!(pb.functions.is_empty());
    }

    #[test]
    fn arrays_extract_in_order_with_quoting_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pkgbuild(
            &dir,
            concat!(
                "pkgname=(zsh zsh-docs)\npkgver=5.9\npkgrel=5\nepoch=1\narch=(x86_64 aarch64)\n",
                "depends=('pcre2' 'ncurses>=6.4' \"lib cap\")\nmakedepends=(yodl)\n",
                "options=(!emptydirs)\n",
            ),
        );
        let pb = extract(&path, dir.path()).unwrap();
        assert_eq!(pb.pkgname, ["zsh", "zsh-docs"]);
        assert_eq!(pb.depends, ["pcre2", "ncurses>=6.4", "lib cap"]);
        assert_eq!(pb.options, ["!emptydirs"]);
        assert_eq!(pb.full_version(), "1:5.9-5");
        assert!(pb.check_arch().is_ok());
    }

    #[test]
    fn functions_are_collected_including_dynamic_pkgver() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pkgbuild(
            &dir,
            concat!(
                "pkgname=dev-thing\npkgver=1.r100\npkgrel=1\narch=(any)\n",
                "pkgver() { echo 1.r101; }\nbuild() { :; }\npackage() { :; }\n",
            ),
        );
        let pb = extract(&path, dir.path()).unwrap();
        assert!(pb.functions.contains("pkgver"));
        assert!(pb.functions.contains("build"));
        assert!(pb.functions.contains("package"));
        // Negative: no phantom prepare().
        assert!(!pb.functions.contains("prepare"));
    }

    #[test]
    fn the_wrong_architecture_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pkgbuild(&dir, "pkgname=armware\npkgver=1\npkgrel=1\narch=(aarch64)\n");
        let pb = extract(&path, dir.path()).unwrap();
        match pb.check_arch().unwrap_err() {
            Error::UnsupportedArch { name, arches } => {
                assert_eq!(name, "armware");
                assert_eq!(arches, ["aarch64"]);
            }
            other => panic!("expected UnsupportedArch, got {other:?}"),
        }
    }

    #[test]
    fn a_failing_pkgbuild_is_an_extraction_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pkgbuild(&dir, "pkgname=broken\necho 'this file is hostile' >&2\nexit 1\n");
        match extract(&path, dir.path()).unwrap_err() {
            Error::Extraction(msg) => assert!(msg.contains("hostile"), "stderr surfaces: {msg}"),
            other => panic!("expected Extraction, got {other:?}"),
        }
    }

    #[test]
    fn evaluation_sees_the_scrubbed_home_not_the_callers() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pkgbuild(
            &dir,
            "pkgname=homer\npkgver=1\npkgrel=1\narch=(any)\nsource=(\"$HOME/tarball.tar.gz\")\n",
        );
        let pb = extract(&path, dir.path()).unwrap();
        let expected = format!("{}/tarball.tar.gz", dir.path().display());
        assert_eq!(pb.source, [expected.clone()]);
        // Negative: the caller's real HOME leaked nowhere.
        let real_home = std::env::var("HOME").unwrap();
        assert_ne!(real_home, dir.path().display().to_string());
        assert!(!pb.source[0].starts_with(&real_home));
    }

    #[test]
    fn missing_pkgver_refuses_and_missing_pkgrel_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_pkgbuild(&dir, "pkgname=nover\narch=(any)\n");
        assert!(matches!(extract(&path, dir.path()).unwrap_err(), Error::Extraction(_)));

        let path = write_pkgbuild(&dir, "pkgname=norel\npkgver=2\narch=(any)\n");
        assert_eq!(extract(&path, dir.path()).unwrap().full_version(), "2-1");
    }
}
