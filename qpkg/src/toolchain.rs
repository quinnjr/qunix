//! The gcc→LLVM substitution. Environment injection does almost all of the
//! work — well-behaved build systems honour CC/CXX/AR — and a small, logged
//! set of textual rules catches PKGBUILDs that hardcode tool names. Every
//! textual rewrite is reported to the caller; none are silent.

use std::path::Path;

use crate::error::{Error, Result};

pub const TARGET: &str = "x86_64-unknown-linux-musl";
/// Arch's musl package installs its sysroot here.
pub const MUSL_ROOT: &str = "/usr/lib/musl";

const REQUIRED_TOOLS: &[&str] =
    &["bash", "git", "clang", "clang++", "llvm-ar", "llvm-ranlib", "llvm-strip", "ld.lld"];

/// The scrubbed environment every build function runs under. The proxies
/// point at a dead local port: a soft network barrier for the build phase —
/// sources were already fetched and verified, so any traffic from build() is
/// traffic we want to fail. (A namespace cut is deliberately out of scope.)
pub fn build_env(workdir: &Path) -> Vec<(String, String)> {
    let cflags = format!("--target={TARGET} --sysroot={MUSL_ROOT} -static -O2");
    let ldflags = format!("--target={TARGET} --sysroot={MUSL_ROOT} -fuse-ld=lld -static");
    let dead = "http://127.0.0.1:9";
    vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), workdir.display().to_string()),
        ("LC_ALL".into(), "C".into()),
        ("CC".into(), "clang".into()),
        ("CXX".into(), "clang++".into()),
        ("AR".into(), "llvm-ar".into()),
        ("RANLIB".into(), "llvm-ranlib".into()),
        ("NM".into(), "llvm-nm".into()),
        ("STRIP".into(), "llvm-strip".into()),
        ("OBJCOPY".into(), "llvm-objcopy".into()),
        ("LD".into(), "ld.lld".into()),
        ("CHOST".into(), TARGET.into()),
        ("CFLAGS".into(), cflags.clone()),
        ("CXXFLAGS".into(), cflags),
        ("LDFLAGS".into(), ldflags),
        ("http_proxy".into(), dead.into()),
        ("https_proxy".into(), dead.into()),
        ("ftp_proxy".into(), dead.into()),
    ]
}

#[derive(Debug, PartialEq, Eq)]
pub struct RewriteLog {
    pub line: usize,
    pub rule: &'static str,
    pub before: String,
    pub after: String,
}

/// Rewrites hardcoded GNU tool invocations in a PKGBUILD's text. Word
/// boundaries treat `-`, `+`, `.` and `_` as word characters, which is what
/// keeps `gcc-libs` (a package name) and `gccgo` untouched while `CC=gcc`
/// and `gcc -o` rewrite. `ar` and `strip` are too short to trust as bare
/// words, so they only rewrite in command position.
pub fn rewrite(text: &str) -> (String, Vec<RewriteLog>) {
    let mut log = Vec::new();
    let mut out_lines = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let mut current = line.to_string();
        if line.trim_start().starts_with('#') {
            out_lines.push(current);
            continue;
        }
        for (rule, from, to, command_position) in [
            ("gcc→clang", "gcc", "clang", false),
            ("g++→clang++", "g++", "clang++", false),
            ("ar→llvm-ar", "ar", "llvm-ar", true),
            ("strip→llvm-strip", "strip", "llvm-strip", true),
        ] {
            let rewritten = replace_tool(&current, from, to, command_position);
            if rewritten != current {
                log.push(RewriteLog {
                    line: idx + 1,
                    rule,
                    before: line.to_string(),
                    after: rewritten.clone(),
                });
                current = rewritten;
            }
        }
        out_lines.push(current);
    }
    let mut out = out_lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    (out, log)
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.')
}

fn replace_tool(line: &str, from: &str, to: &str, command_position: bool) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    let mut consumed = 0usize;
    while let Some(pos) = rest.find(from) {
        let abs = consumed + pos;
        let before_ok = {
            let prev = line[..abs].chars().next_back();
            match prev {
                None => true,
                Some(c) if is_word(c) => false,
                Some(_) if !command_position => true,
                Some(_) => {
                    // Command position: only whitespace may sit between the
                    // token and the start of a command — line start, a pipe,
                    // a separator, or a subshell opener.
                    let lead = line[..abs].trim_end_matches([' ', '\t']);
                    lead.is_empty()
                        || lead.ends_with(['|', ';', '&', '(', '`'])
                }
            }
        };
        let after = line[abs + from.len()..].chars().next();
        let after_ok = match after {
            None => !command_position,
            Some(c) if is_word(c) => false,
            // A command needs arguments; `ar` at end-of-line is not one.
            Some(_) => !command_position || after == Some(' ') || after == Some('\t'),
        };
        out.push_str(&rest[..pos]);
        if before_ok && after_ok {
            out.push_str(to);
        } else {
            out.push_str(from);
        }
        rest = &rest[pos + from.len()..];
        consumed = abs + from.len();
    }
    out.push_str(rest);
    out
}

/// Verifies the host can attempt a cross build at all, before any sources
/// are fetched: the musl sysroot (named by the package to install when
/// absent), then every LLVM tool the injected environment references.
pub fn check_host(musl_root: &Path) -> Result<()> {
    if !musl_root.is_dir() {
        return Err(Error::MissingTool(format!(
            "musl sysroot at {} (pacman -S musl)",
            musl_root.display()
        )));
    }
    tools_present(REQUIRED_TOOLS)
}

fn tools_present(tools: &[&str]) -> Result<()> {
    for tool in tools {
        let found = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join(tool).is_file())
        });
        if !found {
            return Err(Error::MissingTool((*tool).into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_carries_the_cross_target() {
        let dir = tempfile::tempdir().unwrap();
        let env = build_env(dir.path());
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap();
        assert_eq!(get("CC"), "clang");
        assert_eq!(get("CXX"), "clang++");
        assert_eq!(get("CHOST"), TARGET);
        assert!(get("CFLAGS").contains("--target=x86_64-unknown-linux-musl"));
        assert!(get("CFLAGS").contains("-static"));
        assert!(get("LDFLAGS").contains("-fuse-ld=lld"));
        assert_eq!(get("HOME"), &dir.path().display().to_string());
        // The build phase's network points at a dead port.
        assert_eq!(get("http_proxy"), "http://127.0.0.1:9");
    }

    #[test]
    fn hardcoded_compilers_rewrite_and_are_logged() {
        let (out, log) = rewrite("build() {\n  gcc -o hello hello.c\n  ./configure CC=gcc CXX=g++\n}\n");
        assert!(out.contains("clang -o hello hello.c"), "{out}");
        assert!(out.contains("CC=clang CXX=clang++"), "{out}");
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].line, 2);
        assert_eq!(log[0].rule, "gcc→clang");
        assert!(log[0].before.contains("gcc"));
        assert!(log[0].after.contains("clang"));
    }

    #[test]
    fn ar_and_strip_rewrite_only_in_command_position() {
        let (out, _) = rewrite("  ar rcs libfoo.a foo.o\n  llvm-ar rcs x.a x.o && strip x\n");
        assert!(out.contains("llvm-ar rcs libfoo.a foo.o"), "{out}");
        assert!(out.contains("&& llvm-strip x"), "{out}");
        // Negative: "ar" inside words or arguments stays put — and the
        // already-correct llvm-ar did not become llvm-llvm-ar.
        let (out, log) = rewrite("tar xf archive.tar\nmake target strip=none\necho ar\n");
        assert!(out.contains("tar xf archive.tar"), "{out}");
        assert!(out.contains("strip=none"), "{out}");
        assert!(out.contains("echo ar"), "{out}");
        assert!(log.is_empty(), "{log:?}");
    }

    #[test]
    fn lookalikes_and_comments_are_untouched() {
        let text = "depends=(gcc-libs)\nmakedepends=(gccgo libgcc)\n# needs gcc 12 or newer\n";
        let (out, log) = rewrite(text);
        // Byte-identical: no rewrite fired at all.
        assert_eq!(out, text);
        assert!(log.is_empty(), "{log:?}");
    }

    #[test]
    fn missing_musl_sysroot_names_the_package_to_install() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_host(&dir.path().join("no-musl-here")).unwrap_err();
        match err {
            Error::MissingTool(msg) => assert!(msg.contains("pacman -S musl"), "{msg}"),
            other => panic!("expected MissingTool, got {other:?}"),
        }
    }

    #[test]
    fn tool_presence_is_a_real_path_lookup() {
        tools_present(&["sh"]).unwrap();
        // Negative: an absent tool is named.
        match tools_present(&["qpkg-no-such-tool-exists"]).unwrap_err() {
            Error::MissingTool(name) => assert_eq!(name, "qpkg-no-such-tool-exists"),
            other => panic!("expected MissingTool, got {other:?}"),
        }
    }
}
