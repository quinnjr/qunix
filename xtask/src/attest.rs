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
/// The repository these rules are the rules *of*.
///
/// A fork is a different repository with a different branch set, and holding it
/// to this one's layout produces failures its owner cannot fix.
const UPSTREAM_REPO: &str = "quinnjr/qunix";

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

/// Commits to inspect: everything on this branch that the base does not have.
///
/// `main` is tried after `develop` because a fork frequently has only `main` —
/// GitHub creates a fork with the default branch alone unless the contributor
/// asks otherwise — and comparing against it still bounds the range to the
/// contributor's own commits, which is all this gate reads.
///
/// Defaults rather than failing when there is no base at all (a fresh clone of a
/// single branch, or CI in a detached checkout) — refusing to run is worse than
/// checking nothing, because it turns a missing ref into a red build for a
/// contributor who has done nothing wrong.
fn default_range(root: &Path) -> Option<String> {
    for base in ["origin/develop", "develop", "origin/main", "main"] {
        if git(root, &["rev-parse", "--verify", "--quiet", base]).is_ok() {
            return Some(format!("{base}..HEAD"));
        }
    }
    None
}

/// The branch this checkout represents, for the human-readable output only.
///
/// `rev-parse --abbrev-ref HEAD` answers `HEAD` in a detached checkout, which
/// is what `actions/checkout` produces on every run, so the CI-provided names
/// are consulted as a fallback — in the order that makes a pull request report
/// its *source* branch rather than `<n>/merge`.
///
/// Deliberately **not** what the protected-branch rule is evaluated against;
/// see [`protected_candidate`] for why `GITHUB_HEAD_REF` must not reach it.
fn current_branch(named: &str) -> String {
    let head_ref = std::env::var("GITHUB_HEAD_REF").ok();
    let ref_name = std::env::var("GITHUB_REF_NAME").ok();
    resolve_branch(named, head_ref.as_deref(), ref_name.as_deref())
}

/// The branch the protected rule is evaluated against.
///
/// `GITHUB_HEAD_REF` is excluded, and that is the whole point of the function.
/// On a `pull_request` event it holds the *contributor's fork* branch name, and
/// fork contributors routinely work on their fork's `main` or `develop`. Feeding
/// it to the rule failed those pull requests with an instruction — `git reset
/// --hard origin/main` — that would have destroyed the branch under review. The
/// rule is about refs in *this* repository; on a pull request the only such ref
/// is `GITHUB_BASE_REF`, which a pull request is allowed to target.
fn protected_candidate(named: &str) -> String {
    let ref_name = std::env::var("GITHUB_REF_NAME").ok();
    resolve_branch(named, None, ref_name.as_deref())
}

/// Whether the protected-branch rule applies to this event at all.
///
/// Only a push can put commits *on* a protected branch. A `pull_request` event
/// merely proposes them, and evaluating the rule there judges the contributor's
/// own repository (see [`protected_candidate`]). An unset value is a local run,
/// where the rule is exactly the thing a developer wants told about.
fn protected_applies(event: Option<&str>) -> bool {
    event.is_none_or(|e| {
        let e = e.trim();
        e.is_empty() || e == "push"
    })
}

/// Picks the branch name from what git and CI each report.
///
/// Pure, so the precedence can be tested: git's answer wins whenever it is a
/// real name, and `GITHUB_HEAD_REF` is preferred over `GITHUB_REF_NAME` because
/// on a pull request the former is the source branch and the latter is
/// `<n>/merge`. Falls back to the literal `HEAD` when nothing names a branch,
/// which is never protected and so fails open rather than blocking a
/// contributor.
fn resolve_branch(head: &str, head_ref: Option<&str>, ref_name: Option<&str>) -> String {
    if head != "HEAD" {
        return head.to_string();
    }
    for candidate in [head_ref, ref_name].into_iter().flatten() {
        let trimmed = candidate.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    head.to_string()
}

/// The previous head a push event reports, if it names a real commit.
///
/// Split out because the two rejections are easy to get wrong and impossible to
/// exercise through `protected_range`, which needs a repository and a set
/// environment variable. An empty value means the variable was exported but
/// unset; the all-zero sha is git's "this ref did not exist", which a branch
/// creation carries and which no range can be built from.
fn usable_before(raw: &str) -> Option<&str> {
    let before = raw.trim();
    if before.is_empty() || before.chars().all(|c| c == '0') {
        return None;
    }
    Some(before)
}

/// The `before` field of a push event payload, if it is present as a string.
///
/// GitHub does **not** export a `GITHUB_EVENT_BEFORE` variable — `github.event
/// .before` exists only in the expression context and in the payload JSON — so
/// the earlier version of this read a variable nothing sets, `protected_range`
/// returned `None` on every run, and the gate below fell through to a range that
/// is empty by construction on a protected branch. It passed the one push it
/// exists to catch. The payload file is written for every event and needs no
/// per-step `env:` plumbing a new job could forget.
///
/// Hand-rolled rather than pulling in a JSON dependency for one field: only a
/// top-level string is accepted, so a `"before"` nested inside a commit object
/// cannot be mistaken for the push's own.
fn payload_before(json: &str) -> Option<&str> {
    const KEY: &str = "\"before\"";
    let mut from = 0usize;
    while let Some(found) = json[from..].find(KEY) {
        let at = from + found;
        from = at + KEY.len();
        // A key sits directly after the object's `{` or a `,`. Without this a
        // *value* spelled "before" would match.
        let preceded_by_key_position = json[..at]
            .trim_end()
            .chars()
            .next_back()
            .is_some_and(|c| c == '{' || c == ',');
        if !preceded_by_key_position {
            continue;
        }
        let rest = json[from..].trim_start();
        let Some(value) = rest.strip_prefix(':') else { continue };
        // `null` and any non-string are treated as absent, which is the same
        // "nothing to compare against" as a missing key.
        let value = value.trim_start();
        let Some(value) = value.strip_prefix('"') else { continue };
        let end = value.find('"')?;
        return Some(&value[..end]);
    }
    None
}

/// Whether a commit body carries a non-empty attestation trailer.
///
/// Lifted out of `check` because the test module held a *copy* of it. A copy
/// pins nothing: `check`'s own predicate could be inverted, or lose the
/// non-empty test, and every test would still pass against the copy. The tests
/// below now call this function, so they fail when the enforced rule changes.
fn attested(body: &str) -> bool {
    body.lines()
        .map(str::trim)
        .any(|line| line.starts_with(TRAILER) && !line[TRAILER.len()..].trim().is_empty())
}

/// Whether the protected-branch rule fires for this event and branch.
///
/// The two halves were evaluated inline and only the halves were tested, so
/// nothing asserted the *conjunction*: dropping either one leaves both of the
/// existing predicate tests green while the gate either stops firing on a real
/// push or starts failing pull requests from a fork whose branch is named
/// `main` — the latter with an instruction that destroys the branch under
/// review. Both are one-line edits.
fn protected_fires(event: Option<&str>, branch: &str) -> bool {
    protected_applies(event) && PROTECTED.contains(&branch)
}

/// The range a reported previous head implies, given a way to ask whether this
/// checkout contains a commit.
///
/// The git call is a parameter so the fail-*closed* direction is testable at
/// all. It is the direction that matters: an unreachable `before` means a
/// force-push, a rebase, or a shallow clone, and a force-push is precisely how
/// someone circumvents this rule. It used to land in the `None` arm, whose
/// caller then fell back to a ref-based range that is empty by construction on
/// a protected branch — so the gate reported success on the one event it exists
/// to catch.
fn range_for_before(before: Option<&str>, contains: &dyn Fn(&str) -> bool) -> Result<Option<String>> {
    let Some(before) = before.and_then(usable_before) else { return Ok(None) };
    if !contains(before) {
        bail!(
            "the push event names `{before}` as the branch's previous head, but this \
             checkout does not contain it, so what the push added cannot be determined. \
             That is a force-push, a rebase, or a shallow clone -- use `fetch-depth: 0`. \
             Refusing rather than passing: a force-push is how this rule gets circumvented."
        );
    }
    Ok(Some(format!("{before}..HEAD")))
}

/// What a push actually added to a protected branch, when the event says so.
///
/// The previous head is the only reliable way to see commits pushed *straight
/// to* a protected branch: by the time CI runs, `origin/<branch>` already
/// contains them, so every ref-based range is empty.
///
/// `Ok(None)` means "no push event, or a branch creation" — nothing to compare
/// and nothing wrong. A `before` that names a commit this checkout does not
/// contain is an **error**, not a skip: a force-push is the most likely way
/// someone circumvents git-flow, and it used to land in the `None` arm, whose
/// fallback then read an empty range and reported success.
fn protected_range(root: &Path) -> Result<Option<String>> {
    let Ok(path) = std::env::var("GITHUB_EVENT_PATH") else { return Ok(None) };
    let payload = std::fs::read_to_string(&path)
        .with_context(|| format!("reading the event payload at {path}"))?;
    range_for_before(payload_before(&payload), &|before| {
        git(root, &["rev-parse", "--verify", "--quiet", &format!("{before}^{{commit}}")]).is_ok()
    })
}

/// Whether this is running in automation.
///
/// Every mainstream CI sets `CI`. Used only to decide whether a *missing*
/// baseline is a skip or a failure.
fn in_ci() -> bool {
    is_ci_value(std::env::var("CI").ok().as_deref())
}

/// Whether a `CI` value means "running in automation".
///
/// Split out because the interesting cases are the falsy ones: some tools
/// export `CI=` or `CI=false` rather than leaving it unset, and treating either
/// as true turns a local run into a hard failure on a missing `develop`.
fn is_ci_value(value: Option<&str>) -> bool {
    matches!(value, Some(v) if !v.is_empty() && v != "false")
}

/// Whether a checkout with no base ref to compare against is a hard failure.
///
/// On the upstream repository it is: `develop` exists there, so its absence
/// means the workflow fetched wrongly and the gate would pass everything.
/// On a fork it is not. A fork created from the default branch alone has no
/// `develop`, `main` may be its only ref, and the contributor cannot add one
/// that this workflow would see — failing there is a red build for a repository
/// state nobody can fix. `GITHUB_REPOSITORY` names the repository the run
/// belongs to, and is `<fork-owner>/<name>` for a fork.
fn missing_base_is_fatal(ci: bool, repository: Option<&str>) -> bool {
    ci && repository.is_none_or(|r| {
        let r = r.trim();
        r.is_empty() || r == UPSTREAM_REPO
    })
}

/// Checks attestation trailers, and that work is not sitting on a protected branch.
pub fn check(root: &Path, range: Option<&str>) -> Result<()> {
    let named = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch = current_branch(&named);

    let Some(range) = range.map(str::to_string).or_else(|| default_range(root)) else {
        // Locally this is a fresh single-branch clone and checking nothing beats
        // a red build. On the upstream repository in CI it means the checkout
        // has neither `develop` nor `main` to compare against, so the gate would
        // pass every commit unconditionally -- which is the failure mode a gate
        // must never have. `fetch-depth: 0` is what fixes it on the workflow
        // side. On a fork the same state is not the contributor's doing, so it
        // warns instead; see `missing_base_is_fatal`.
        let repository = std::env::var("GITHUB_REPOSITORY").ok();
        if missing_base_is_fatal(in_ci(), repository.as_deref()) {
            bail!(
                "attest: no `develop` or `main` ref in this checkout, so nothing can be \
                 verified. CI must fetch one (`fetch-depth: 0`) rather than skip the check."
            );
        }
        println!("attest: no develop or main ref to compare against, skipping");
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

    // The protected-branch rule is evaluated *before* the empty-range return,
    // and against its own range. On `develop` the default range is
    // `origin/develop..HEAD`, which is empty the moment the commits are pushed
    // -- so the one situation the rule exists to catch, a direct push to a
    // protected branch, returned early and reported success.
    //
    // Restricted to push (and local) events, and to a branch name that cannot
    // have come from a fork: a pull request from a fork's own `main` is not a
    // commit on *this* repository's `main`.
    let event = std::env::var("GITHUB_EVENT_NAME").ok();
    let protected = protected_candidate(&named);
    if protected_fires(event.as_deref(), &protected) {
        let pushed = protected_range(root)?
            .map(|r| git(root, &["rev-list", "--no-merges", &r]))
            .transpose()?
            .map(|out| out.lines().filter(|l| !l.is_empty()).count())
            .unwrap_or(hashes.len());
        if pushed > 0 {
            bail!(
                "{pushed} commit(s) sit directly on `{protected}`, which git-flow protects. \
                 Move them to a feature branch:\n  \
                 git switch -c feature/<name>\n  \
                 git switch {protected} && git reset --hard origin/{protected}\n\
                 See CONTRIBUTING.md."
            );
        }
    }

    if hashes.is_empty() {
        // Nothing new. The protected-branch rule was already evaluated above,
        // where an empty range does not mean an empty push.
        println!("attest: no commits in {range}");
        return Ok(());
    }

    let mut missing = Vec::new();
    for hash in &hashes {
        let body = git(root, &["show", "-s", "--format=%B", hash])?;
        if !attested(&body) {
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
    // The real predicate `check` calls, not a copy of it. A copy pinned
    // nothing: `check`'s own version could lose the non-empty test and every
    // test here would still pass.
    use super::attested;

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
    fn git_names_the_branch_when_it_can() {
        assert_eq!(super::resolve_branch("develop", Some("feature/x"), Some("y")), "develop");
    }

    #[test]
    fn a_detached_checkout_falls_back_to_the_source_branch() {
        // The order matters: on a pull request `GITHUB_REF_NAME` is `<n>/merge`,
        // which is not a branch anyone can be told to move commits off. This is
        // the case that made the protected-branch check inert in CI, where
        // every checkout is detached and the name read back as `HEAD`.
        assert_eq!(super::resolve_branch("HEAD", Some("feature/x"), Some("7/merge")), "feature/x");
        assert_eq!(super::resolve_branch("HEAD", None, Some("develop")), "develop");
        assert_eq!(super::resolve_branch("HEAD", Some("  "), Some("main")), "main");
        // Nothing names a branch: fail open. `HEAD` is not protected.
        assert_eq!(super::resolve_branch("HEAD", None, None), "HEAD");
    }

    #[test]
    fn a_falsy_ci_value_is_not_ci() {
        // Exported-but-empty and the literal `false` both appear in the wild.
        // Reading either as true makes a local run fail on a missing `develop`.
        assert!(!super::is_ci_value(None));
        assert!(!super::is_ci_value(Some("")));
        assert!(!super::is_ci_value(Some("false")));
        assert!(super::is_ci_value(Some("true")));
        assert!(super::is_ci_value(Some("1")));
    }

    #[test]
    fn an_absent_previous_head_yields_no_range() {
        // Both of git's ways of saying "there was nothing here". Treating
        // either as a commit builds `000000..HEAD`, which `rev-list` rejects --
        // turning a branch creation into a red build rather than a skipped
        // check.
        assert_eq!(super::usable_before(""), None);
        assert_eq!(super::usable_before("   "), None);
        assert_eq!(super::usable_before("0000000000000000000000000000000000000000"), None);
        assert_eq!(super::usable_before("0"), None);
    }

    #[test]
    fn a_real_previous_head_is_used_verbatim_after_trimming() {
        assert_eq!(super::usable_before("abc1234"), Some("abc1234"));
        assert_eq!(super::usable_before("  abc1234\n"), Some("abc1234"));
        // A sha that merely starts with zeros is a real commit.
        assert_eq!(super::usable_before("0abc123"), Some("0abc123"));
    }

    #[test]
    fn the_protected_rule_never_reads_the_fork_branch_name() {
        // The precedence that matters: on a pull request `GITHUB_HEAD_REF` is
        // the *contributor's fork* branch, so a fork whose only branch is `main`
        // would otherwise be told to `git reset --hard origin/main` -- destroying
        // the branch under review. Only git's own name and `GITHUB_REF_NAME` may
        // reach the rule.
        assert_eq!(super::resolve_branch("HEAD", None, Some("7/merge")), "7/merge");
        assert_eq!(super::resolve_branch("feature/x", None, Some("develop")), "feature/x");
        // The informational name still prefers the source branch.
        assert_eq!(super::resolve_branch("HEAD", Some("main"), Some("7/merge")), "main");
    }

    #[test]
    fn the_protected_rule_applies_only_to_pushes_and_local_runs() {
        assert!(super::protected_applies(None));
        assert!(super::protected_applies(Some("")));
        assert!(super::protected_applies(Some("push")));
        // A pull request proposes commits; it does not put them on a branch
        // here, and its head ref belongs to another repository.
        assert!(!super::protected_applies(Some("pull_request")));
        assert!(!super::protected_applies(Some("pull_request_target")));
        assert!(!super::protected_applies(Some("schedule")));
    }

    #[test]
    fn a_fork_without_a_base_ref_is_not_failed() {
        // The state a fork contributor cannot fix: no `develop`, `main` the only
        // branch. Fatal upstream, a warning anywhere else.
        assert!(super::missing_base_is_fatal(true, Some("quinnjr/qunix")));
        assert!(super::missing_base_is_fatal(true, None));
        assert!(!super::missing_base_is_fatal(true, Some("someone-else/qunix")));
        // Locally it was never fatal.
        assert!(!super::missing_base_is_fatal(false, Some("quinnjr/qunix")));
    }

    #[test]
    fn the_previous_head_is_read_out_of_the_event_payload() {
        let payload = r#"{"ref":"refs/heads/develop","before":"abc1234","after":"def5678"}"#;
        assert_eq!(super::payload_before(payload), Some("abc1234"));
        // Whitespace-formatted payloads are what GitHub actually writes.
        assert_eq!(
            super::payload_before("{\n  \"before\" : \"abc1234\" ,\n  \"after\": \"d\"\n}"),
            Some("abc1234")
        );
    }

    #[test]
    fn a_payload_without_a_usable_before_yields_nothing() {
        // Every one of these used to be indistinguishable from "checked".
        assert_eq!(super::payload_before(r#"{"action":"opened"}"#), None);
        assert_eq!(super::payload_before(r#"{"before":null}"#), None);
        // A *value* spelled "before" is not the key.
        assert_eq!(super::payload_before(r#"{"title":"\"before\"","x":1}"#), None);
        // The all-zero sentinel of a branch creation is rejected downstream.
        let created = r#"{"before":"0000000000000000000000000000000000000000"}"#;
        assert_eq!(super::payload_before(created).and_then(super::usable_before), None);
    }

    #[test]
    fn the_previous_head_mechanism_agrees_with_the_workflow() {
        // Cross-checks the source against a file on disk, like
        // `fuzz_targets_sources_and_manifest_all_agree` does for the fuzz
        // manifest. The gate was inert for exactly this reason: it read
        // `GITHUB_EVENT_BEFORE`, which GitHub does not export and which no
        // workflow set, so a direct push to `develop` passed. If someone moves
        // back to the env route, the workflow must carry the `env:` block.
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/attest.rs"))
            .expect("reading attest.rs");
        assert!(
            src.contains("GITHUB_EVENT_PATH"),
            "the previous head is no longer read from the event payload"
        );
        // Only the code above the test module: this test names the variable
        // itself, and matching that would make the check assert on its own text.
        let code = src.split("#[cfg(test)]").next().unwrap_or_default();
        let reads_env_var = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("///"))
            .any(|l| l.contains("GITHUB_EVENT_BEFORE"));
        if reads_env_var {
            let workflow =
                std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../.github/workflows/ci.yml"))
                    .expect("reading ci.yml");
            assert!(
                workflow.contains("GITHUB_EVENT_BEFORE:"),
                "attest reads GITHUB_EVENT_BEFORE but no workflow step exports it, \
                 so the protected-branch gate checks nothing"
            );
        }
    }

    #[test]
    fn a_push_to_a_protected_branch_fires_the_rule() {
        // The conjunction, not the two halves separately. Deleting either one
        // leaves `protected_applies` and `resolve_branch`'s own tests green
        // while the gate stops doing its job -- which is how it came to report
        // success for a direct push to `develop`.
        for branch in super::PROTECTED {
            assert!(super::protected_fires(Some("push"), branch), "a push to {branch} passed");
            // A local run is the case a developer most wants told about.
            assert!(super::protected_fires(None, branch), "a local commit on {branch} passed");
        }
    }

    #[test]
    fn a_fork_pull_request_from_a_branch_named_main_does_not_fire_the_rule() {
        // A fork's own `main` is not this repository's `main`. Firing here told
        // the contributor to `git reset --hard origin/main`, which destroys the
        // branch under review. `protected_candidate` never reads
        // `GITHUB_HEAD_REF`, so the name reaching the rule on a pull request is
        // `<n>/merge` -- but the event test has to hold even if it did.
        assert!(!super::protected_fires(Some("pull_request"), "main"));
        assert!(!super::protected_fires(Some("pull_request_target"), "develop"));
        assert_eq!(super::resolve_branch("HEAD", None, Some("7/merge")), "7/merge");
        assert!(!super::protected_fires(Some("pull_request"), "7/merge"));
    }

    #[test]
    fn a_feature_branch_never_fires_the_rule() {
        // The other direction, or the tests above are satisfied by a gate that
        // fires on everything and blocks all work.
        for branch in ["feature/m2-filesystems", "HEAD", "mainline", "develop-2"] {
            assert!(!super::protected_fires(Some("push"), branch), "{branch} was treated as protected");
        }
    }

    #[test]
    fn an_unreachable_previous_head_fails_closed() {
        // A force-push is the most likely way this rule gets circumvented, and
        // it used to land in the "nothing to compare" arm -- whose caller then
        // read a ref-based range that is empty by construction on a protected
        // branch, and reported success.
        let err = super::range_for_before(Some("abc1234"), &|_| false).unwrap_err();
        assert!(err.to_string().contains("abc1234"), "the sha is not named: {err}");
        assert!(err.to_string().contains("force-push"), "unhelpful error: {err}");
    }

    #[test]
    fn a_reachable_previous_head_bounds_the_range_at_it() {
        // Not `origin/<branch>..HEAD`: by the time CI runs, the remote ref
        // already contains the pushed commits and every ref-based range is
        // empty. The previous head is the only thing that still sees them.
        assert_eq!(
            super::range_for_before(Some("abc1234"), &|sha| sha == "abc1234").unwrap(),
            Some("abc1234..HEAD".to_string())
        );
    }

    #[test]
    fn a_branch_creation_yields_no_range_without_consulting_git() {
        // The all-zero sentinel names no commit, so asking git about it would
        // turn creating a branch into a red build. Nothing may be asked of git
        // here -- the closure fails the test if it is.
        let never = |_: &str| panic!("git was consulted about a sha that names no commit");
        assert_eq!(super::range_for_before(None, &never).unwrap(), None);
        assert_eq!(
            super::range_for_before(Some("0000000000000000000000000000000000000000"), &never).unwrap(),
            None
        );
        assert_eq!(super::range_for_before(Some(""), &never).unwrap(), None);
    }

    #[test]
    fn a_trailer_that_is_only_a_prefix_of_another_does_not_attest() {
        // `starts_with` is the whole test, so a trailer whose key merely begins
        // with `Assisted-by` would satisfy it while saying something else.
        assert!(!attested("feat: thing\n\nCo-Assisted-by: Claude\n"));
        // And the value must be the trailer's, not a continuation line's.
        assert!(!attested("feat: thing\n\nAssisted-by:\n  Claude Opus 5\n"));
    }

    #[test]
    fn multiple_tools_attest() {
        let body = "feat: thing\n\nAssisted-by: Claude Opus 5\nAssisted-by: Copilot\n";
        assert!(attested(body));
    }
}
