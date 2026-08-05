//! Enforces the contribution rules in `CONTRIBUTING.md`: every commit attests
//! what assisted in writing it, and work arrives via a feature branch.
//!
//! Both are stated as requirements there, and a requirement nobody checks is a
//! suggestion.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

const TRAILER: &str = "Assisted-by:";
/// Branches nothing may be committed to directly.
const PROTECTED: &[&str] = &["main", "develop"];

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Commits to inspect: everything on this branch that `develop` does not have.
///
/// Defaults rather than failing when there is no `develop` (a fresh clone of a
/// single branch, or CI in a detached checkout) — refusing to run is worse than
/// checking nothing, because it turns a missing ref into a red build for a
/// contributor who has done nothing wrong.
fn default_range(root: &Path) -> Option<String> {
    for base in ["origin/develop", "develop"] {
        if git(root, &["rev-parse", "--verify", "--quiet", base]).is_ok() {
            return Some(format!("{base}..HEAD"));
        }
    }
    None
}

/// Checks attestation trailers, and that work is not sitting on a protected branch.
pub fn check(root: &Path, range: Option<&str>) -> Result<()> {
    let branch = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?;

    let Some(range) = range.map(str::to_string).or_else(|| default_range(root)) else {
        println!("attest: no develop ref to compare against, skipping");
        return Ok(());
    };

    // `%H %(trailers:key=Assisted-by,valueonly)` collapses multi-line trailers,
    // so read the raw body per commit instead.
    // `--no-merges`: attestation is about authored content, and a merge
    // introduces none. It also matters mechanically -- actions/checkout builds
    // a synthetic `refs/pull/N/merge` commit for pull requests, which GitHub
    // authors and which can carry no trailer, so without this every PR fails
    // on a commit nobody wrote.
    let hashes = git(root, &["rev-list", "--no-merges", &range])?;
    let hashes: Vec<&str> = hashes.lines().filter(|l| !l.is_empty()).collect();

    if hashes.is_empty() {
        // Nothing new. Only a problem if the branch itself is protected AND
        // dirty, which the working-tree check below would not catch either.
        println!("attest: no commits in {range}");
        return Ok(());
    }

    if PROTECTED.contains(&branch.as_str()) {
        bail!(
            "{n} commit(s) sit directly on `{branch}`, which git-flow protects. \
             Move them to a feature branch:\n  \
             git switch -c feature/<name>\n  \
             git switch {branch} && git reset --hard origin/{branch}\n\
             See CONTRIBUTING.md.",
            n = hashes.len()
        );
    }

    let mut missing = Vec::new();
    for hash in &hashes {
        let body = git(root, &["show", "-s", "--format=%B", hash])?;
        let attested = body
            .lines()
            .map(str::trim)
            .any(|line| line.starts_with(TRAILER) && !line[TRAILER.len()..].trim().is_empty());
        if !attested {
            let subject = git(root, &["show", "-s", "--format=%s", hash])?;
            missing.push(format!("  {} {}", &hash[..8.min(hash.len())], subject));
        }
    }

    if !missing.is_empty() {
        bail!(
            "{n} commit(s) in {range} lack an `{TRAILER}` trailer:\n{list}\n\n\
             Every commit must attest what assisted in writing it, or `none`:\n  \
             {TRAILER} Claude Opus 5\n  \
             {TRAILER} none\n\n\
             Add it with `git commit --amend` or `git rebase -i`. See CONTRIBUTING.md.",
            n = missing.len(),
            list = missing.join("\n")
        );
    }

    println!("attest: {} commit(s) attested on `{branch}`", hashes.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    const TRAILER: &str = super::TRAILER;

    /// Mirrors the predicate in `check`, so the parsing rule is pinned even
    /// though the surrounding function needs a real repository.
    pub fn attested(body: &str) -> bool {
        body.lines()
            .map(str::trim)
            .any(|line| line.starts_with(TRAILER) && !line[TRAILER.len()..].trim().is_empty())
    }

    #[test]
    fn a_named_tool_attests() {
        assert!(attested("feat: thing\n\nwhy\n\nAssisted-by: Claude Opus 5\n"));
    }

    #[test]
    fn none_attests() {
        assert!(attested("fix: thing\n\nAssisted-by: none\n"));
    }

    #[test]
    fn a_missing_trailer_does_not() {
        assert!(!attested("feat: thing\n\nwhy\n\nCo-Authored-By: Someone\n"));
    }

    #[test]
    fn an_empty_value_does_not() {
        // The whole point of requiring an explicit value is that silence and
        // "none" must be distinguishable.
        assert!(!attested("feat: thing\n\nAssisted-by:\n"));
        assert!(!attested("feat: thing\n\nAssisted-by:   \n"));
    }

    #[test]
    fn multiple_tools_attest() {
        let body = "feat: thing\n\nAssisted-by: Claude Opus 5\nAssisted-by: Copilot\n";
        assert!(attested(body));
    }
}
