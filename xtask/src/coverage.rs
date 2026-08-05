//! Line-coverage ratchet: a change may raise coverage or hold it, never drop it.
//!
//! Ratcheted **per crate**, not against one aggregate number. The crates here
//! have legitimately different ceilings — `qunix-mm` is pure logic and sits
//! above 90%, while `xtask` mostly shells out to cargo and QEMU and `port.rs`
//! is raw `in`/`out` instructions that cannot execute on the host at all. A
//! single global figure would let a well-tested allocator change be failed by
//! unrelated growth in untestable orchestration, which teaches contributors to
//! game the number rather than test their code.
//!
//! Scope worth stating plainly: this measures the crates that build for the
//! host. The kernel's own 14 in-QEMU tests are invisible to it, so coverage
//! says nothing about `qunix-kernel`.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

const BASELINE: &str = "coverage-baseline.toml";

/// Crates measured, and the feature each needs to build for the host.
const MEASURED: &[(&str, Option<&str>)] = &[
    ("qunix-sync", Some("qunix-sync/std")),
    ("qunix-mm", Some("qunix-mm/std")),
    ("qunix-hal-x86_64", Some("qunix-hal-x86_64/std")),
    ("qunix-sched", Some("qunix-sched/std")),
    ("qunix-elf", Some("qunix-elf/std")),
    ("xtask", None),
];

/// Absorbs the sub-percent drift a pure refactor can cause when lines move
/// between counted and uncounted forms. Wide enough not to churn, far too
/// narrow to hide a genuinely untested function.
const TOLERANCE_PP: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lines {
    pub hit: u64,
    pub found: u64,
}

impl Lines {
    fn percent(&self) -> f64 {
        if self.found == 0 {
            // A crate with no executable lines is vacuously covered; treating it
            // as 0% would make an empty crate permanently unfixable.
            return 100.0;
        }
        self.hit as f64 * 100.0 / self.found as f64
    }
}

/// Uncovered line numbers per crate, as `path:line` strings.
///
/// The ratchet used to report only that a percentage fell, which is not enough
/// to act on -- when a drop appears in CI but not locally, the next question is
/// always *which lines*, and answering it meant guessing or adding a temporary
/// debug step. Now the failure says so directly.
fn uncovered_lines(lcov: &str, root: &Path) -> BTreeMap<String, Vec<String>> {
    let mut per_crate: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut file = String::new();

    for line in lcov.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            current = crate_of(path, root);
            // Path relative to the workspace, not the basename: two `mod.rs`
            // files in one crate are otherwise indistinguishable in the report,
            // which is the report's whole job.
            file = path
                .strip_prefix(root.to_str().unwrap_or_default())
                .unwrap_or(path)
                .trim_start_matches('/')
                .to_string();
        } else if let Some(value) = line.strip_prefix("DA:")
            && let Some((number, rest)) = value.split_once(',')
            // `DA:<line>,<count>[,<checksum>]`. Comparing everything after the
            // first comma against "0" silently drops every line when lcov is
            // emitted with checksums -- a silent miss in the one tool whose
            // purpose is to stop people guessing.
            && rest.split(',').next().and_then(|h| h.parse::<u64>().ok()) == Some(0)
            && let Some(name) = &current
        {
            per_crate.entry(name.clone()).or_default().push(format!("{file}:{number}"));
        }
    }
    per_crate
}

/// Sums `LF:`/`LH:` records per crate.
///
/// lcov is used rather than the human summary because its record format is
/// line-oriented and stable across cargo-llvm-cov versions, where the table
/// layout is not.
pub fn parse_lcov(lcov: &str, root: &Path) -> BTreeMap<String, Lines> {
    let mut per_crate: BTreeMap<String, Lines> = BTreeMap::new();
    let mut current: Option<String> = None;

    for line in lcov.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            current = crate_of(path, root);
        } else if let Some(value) = line.strip_prefix("LF:")
            && let (Some(name), Ok(found)) = (&current, value.parse::<u64>())
        {
            per_crate.entry(name.clone()).or_insert(Lines { hit: 0, found: 0 }).found += found;
        } else if let Some(value) = line.strip_prefix("LH:")
            && let (Some(name), Ok(hit)) = (&current, value.parse::<u64>())
        {
            per_crate.entry(name.clone()).or_insert(Lines { hit: 0, found: 0 }).hit += hit;
        }
    }
    per_crate
}

/// Maps a source path to the crate that owns it, or `None` for anything outside
/// the workspace (dependencies, the standard library).
fn crate_of(path: &str, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root.to_str()?)?.trim_start_matches('/');
    let name = if let Some(rest) = rel.strip_prefix("crates/") {
        rest.split('/').next()?
    } else {
        rel.split('/').next()?
    };
    MEASURED
        .iter()
        .find(|(measured, _)| *measured == name)
        .map(|(measured, _)| (*measured).to_string())
}

fn measure(root: &Path) -> Result<(BTreeMap<String, Lines>, String)> {
    let out_path = root.join("target/coverage.lcov");
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let features: Vec<&str> = MEASURED.iter().filter_map(|(_, f)| *f).collect();
    let mut cmd = Command::new(env!("CARGO"));
    cmd.current_dir(root);
    cmd.args(["llvm-cov", "--lcov", "--output-path"]);
    cmd.arg(&out_path);
    for (name, _) in MEASURED {
        cmd.args(["-p", name]);
    }
    cmd.args(["--features", &features.join(",")]);
    cmd.args(["--target", "x86_64-unknown-linux-musl"]);

    let status = cmd.status().context(
        "failed to run cargo llvm-cov; install it with `cargo install cargo-llvm-cov`",
    )?;
    if !status.success() {
        bail!("cargo llvm-cov failed");
    }

    let lcov = std::fs::read_to_string(&out_path)
        .with_context(|| format!("reading {}", out_path.display()))?;
    let measured = parse_lcov(&lcov, root);

    // A parser that silently matches nothing would report every crate as
    // missing, which reads as "coverage collapsed" rather than "the tool broke".
    if measured.is_empty() {
        bail!(
            "coverage run produced no per-crate data from {}; \
             the lcov format or the workspace layout changed",
            out_path.display()
        );
    }
    Ok((measured, lcov))
}

pub fn parse_baseline(text: &str) -> BTreeMap<String, f64> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            let (name, value) = line.split_once('=')?;
            let percent = value.trim().parse::<f64>().ok()?;
            Some((name.trim().trim_matches('"').to_string(), percent))
        })
        .collect()
}

/// Per-crate comment lines from an existing baseline, keyed by crate.
///
/// `--update` used to rewrite the file from scratch, which silently deleted the
/// recorded reason a floor was where it was — including, once, a reason that
/// was known to be wrong, so the record of the mistake vanished rather than
/// being corrected. CONTRIBUTING requires that reason to exist; the tool must
/// therefore preserve it rather than rely on nobody running `--update`.
fn existing_notes(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut pending: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            pending.push(line.to_string());
        } else if let Some((name, _)) = trimmed.split_once('=') {
            if !pending.is_empty() {
                notes.insert(name.trim().trim_matches('"').to_string(), std::mem::take(&mut pending));
            }
        } else if trimmed.is_empty() {
            // A blank line ends a block, so the file header is not attributed
            // to whichever crate happens to be listed first.
            pending.clear();
        }
    }
    notes
}

fn render_baseline(measured: &BTreeMap<String, Lines>, notes: &BTreeMap<String, Vec<String>>) -> String {
    let mut out = String::from(
        "# Per-crate line coverage floor. A change may raise these or hold them,\n\
         # never drop them by more than the tolerance in xtask/src/coverage.rs.\n\
         #\n\
         # Regenerate with `cargo xtask coverage --update` when coverage\n\
         # genuinely improves, or when a crate gains code that cannot be covered\n\
         # at all -- xtask command dispatch that only spawns cargo is the usual\n\
         # case. The second kind of update belongs in a PR that says which code\n\
         # is uncoverable and why.\n",
    );
    for (name, lines) in measured {
        out.push('\n');
        for note in notes.get(name).into_iter().flatten() {
            out.push_str(note);
            out.push('\n');
        }
        out.push_str(&format!("{name} = {:.2}\n", lines.percent()));
    }
    out
}

/// Formats the uncovered lines of each regressed crate.
///
/// A regressed crate with no parsed lines gets an explicit marker rather than
/// silence: an empty section is indistinguishable from "this crate regressed
/// while having zero uncovered lines", which cannot happen, so the difference
/// must be visible or the diagnostic quietly reverts to a bare percentage.
fn render_uncovered(regressed: &[String], uncovered: &BTreeMap<String, Vec<String>>) -> String {
    let mut detail = String::new();
    for name in regressed {
        match uncovered.get(name) {
            Some(lines) => {
                detail.push_str(&format!("\n  {name} uncovered ({}):\n", lines.len()));
                for chunk in lines.chunks(4) {
                    detail.push_str(&format!("    {}\n", chunk.join(" ")));
                }
            }
            None => detail.push_str(&format!(
                "\n  {name}: no uncovered lines parsed -- the lcov DA format or the \
                 workspace layout has changed, so this list is missing, not empty\n"
            )),
        }
    }
    detail
}

/// Measures coverage and compares it against the committed floor.
pub fn check(root: &Path, update: bool) -> Result<()> {
    let (measured, lcov) = measure(root)?;
    let baseline_path = root.join(BASELINE);

    if update {
        // Read before write so per-crate rationales survive the rewrite.
        let previous = std::fs::read_to_string(&baseline_path).unwrap_or_default();
        let notes = existing_notes(&previous);
        // A floor may only go *down* with a recorded reason. Enforced rather
        // than requested: the last time this was a convention, a floor was
        // lowered on a justification that turned out to be false, and nothing
        // caught it.
        let old = parse_baseline(&previous);
        let undocumented: Vec<String> = measured
            .iter()
            .filter(|(name, lines)| {
                old.get(*name).is_some_and(|floor| lines.percent() + TOLERANCE_PP < *floor)
                    && !notes.contains_key(*name)
            })
            .map(|(name, _)| name.clone())
            .collect();
        if !undocumented.is_empty() {
            bail!(
                "refusing to lower the floor for {} without a reason.\n\n\
                 Add a `#` comment directly above that crate's line in {BASELINE} saying \
                 which code is uncoverable and why, then re-run. Lowering a floor is a \
                 decision to argue for, not a side effect of running --update.",
                undocumented.join(", ")
            );
        }
        std::fs::write(&baseline_path, render_baseline(&measured, &notes))?;
        println!("coverage: baseline written to {BASELINE}");
        for (name, lines) in &measured {
            println!("  {name:<20} {:>6.2}%  ({}/{})", lines.percent(), lines.hit, lines.found);
        }
        return Ok(());
    }

    let baseline_text = std::fs::read_to_string(&baseline_path).with_context(|| {
        format!("reading {BASELINE}; create it with `cargo xtask coverage --update`")
    })?;
    let baseline = parse_baseline(&baseline_text);

    let mut regressions = Vec::new();
    let mut regressed_names: Vec<String> = Vec::new();
    let mut improvements = Vec::new();

    for (name, lines) in &measured {
        let now = lines.percent();
        let Some(&floor) = baseline.get(name) else {
            // A new crate has no floor yet. Report it rather than failing, so
            // adding a crate is not blocked by a file it cannot have updated.
            improvements.push(format!("  {name:<20} {now:>6.2}%  (new, no floor recorded)"));
            continue;
        };
        if now + TOLERANCE_PP < floor {
            regressions.push(format!(
                "  {name:<20} {now:>6.2}%  floor {floor:.2}%  ({:+.2} pp)",
                now - floor
            ));
            regressed_names.push(name.clone());
        } else if now > floor + TOLERANCE_PP {
            improvements.push(format!(
                "  {name:<20} {now:>6.2}%  floor {floor:.2}%  ({:+.2} pp)",
                now - floor
            ));
        }
    }

    if !improvements.is_empty() {
        println!("coverage improved:\n{}", improvements.join("\n"));
        println!("  run `cargo xtask coverage --update` to raise the floor");
    }

    if !regressions.is_empty() {
        // Name the lines, not just the number. Without this a drop that only
        // reproduces on CI hardware cannot be diagnosed from the log.
        let detail = render_uncovered(&regressed_names, &uncovered_lines(&lcov, root));
        bail!(
            "coverage dropped in {n} crate(s):\n{list}\n\n\
             Add tests for the new code. If it genuinely cannot be covered — \
             `xtask` command dispatch that only spawns cargo is the usual case \
             — run `cargo xtask coverage --update` and say in the PR which code \
             is uncoverable and why. Lowering the floor is a decision to be \
             argued for, not a way to make a red build green quietly.{detail}",
            n = regressions.len(),
            list = regressions.join("\n")
        );
    }

    println!("coverage: {} crate(s) at or above their floor", measured.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from("/w")
    }

    #[test]
    fn lcov_sums_files_into_their_crate() {
        let lcov = "\
SF:/w/crates/qunix-mm/src/buddy.rs
LF:100
LH:90
end_of_record
SF:/w/crates/qunix-mm/src/slab.rs
LF:100
LH:80
end_of_record
";
        let per_crate = parse_lcov(lcov, &root());
        assert_eq!(per_crate["qunix-mm"], Lines { hit: 170, found: 200 });
        assert_eq!(per_crate["qunix-mm"].percent(), 85.0);
    }

    #[test]
    fn paths_outside_the_workspace_are_ignored() {
        // Dependency and std sources appear in lcov and must not dilute a crate.
        let lcov = "\
SF:/home/user/.cargo/registry/src/x86_64-0.15/src/lib.rs
LF:1000
LH:0
end_of_record
SF:/w/crates/qunix-sync/src/lib.rs
LF:10
LH:10
end_of_record
";
        let per_crate = parse_lcov(lcov, &root());
        assert_eq!(per_crate.len(), 1);
        assert_eq!(per_crate["qunix-sync"].percent(), 100.0);
    }

    #[test]
    fn a_non_workspace_crate_directory_is_not_measured() {
        let lcov = "SF:/w/crates/not-a-real-crate/src/lib.rs\nLF:10\nLH:0\nend_of_record\n";
        assert!(parse_lcov(lcov, &root()).is_empty());
    }

    #[test]
    fn xtask_is_measured_despite_not_living_under_crates() {
        let lcov = "SF:/w/xtask/src/main.rs\nLF:10\nLH:5\nend_of_record\n";
        let per_crate = parse_lcov(lcov, &root());
        assert_eq!(per_crate["xtask"].percent(), 50.0);
    }

    #[test]
    fn a_crate_with_no_executable_lines_is_vacuously_covered() {
        // Otherwise an empty crate reads as 0% and can never reach its floor.
        assert_eq!(Lines { hit: 0, found: 0 }.percent(), 100.0);
    }

    #[test]
    fn baseline_parsing_skips_comments_and_blanks() {
        let text = "# a comment\n\nqunix-mm = 95.08\nxtask = 30.5\n";
        let baseline = parse_baseline(text);
        assert_eq!(baseline["qunix-mm"], 95.08);
        assert_eq!(baseline["xtask"], 30.5);
        assert_eq!(baseline.len(), 2);
    }

    #[test]
    fn uncovered_lines_tolerates_a_da_record_with_a_checksum() {
        // `DA:<line>,<count>[,<checksum>]`. Comparing the whole tail against
        // "0" drops every line the moment lcov emits checksums.
        let lcov = "SF:/w/crates/qunix-mm/src/buddy.rs\nDA:7,0,a1b2c3\nDA:8,3,d4e5\nend_of_record\n";
        assert_eq!(uncovered_lines(lcov, &root())["qunix-mm"], vec!["crates/qunix-mm/src/buddy.rs:7"]);
    }

    #[test]
    fn uncovered_lines_disambiguates_two_files_with_the_same_basename() {
        let lcov = "\
SF:/w/crates/qunix-mm/src/a/mod.rs
DA:12,0
end_of_record
SF:/w/crates/qunix-mm/src/b/mod.rs
DA:12,0
end_of_record
";
        let lines = &uncovered_lines(lcov, &root())["qunix-mm"];
        assert_ne!(lines[0], lines[1], "two different files reported identically: {lines:?}");
    }

    #[test]
    fn uncovered_lines_agrees_with_parse_lcov_on_the_miss_count() {
        // Ties the two parsers together so either one drifting is a failure.
        let lcov = "\
SF:/w/crates/qunix-sync/src/lib.rs
DA:1,1
DA:2,0
DA:3,0
LF:3
LH:1
end_of_record
";
        let totals = parse_lcov(lcov, &root());
        let uncovered = uncovered_lines(lcov, &root());
        let t = totals["qunix-sync"];
        assert_eq!(uncovered["qunix-sync"].len() as u64, t.found - t.hit);
    }

    #[test]
    fn render_uncovered_says_so_when_no_lines_were_parsed() {
        // The negative direction: silence here reads as "regressed with zero
        // uncovered lines", which is impossible, so it must be stated.
        let out = render_uncovered(&["qunix-mm".to_string()], &BTreeMap::new());
        assert!(out.contains("no uncovered lines parsed"), "silent empty section: {out:?}");
    }

    #[test]
    fn render_uncovered_names_the_crate_and_its_lines() {
        let mut map = BTreeMap::new();
        map.insert("qunix-mm".to_string(), vec!["src/buddy.rs:7".to_string()]);
        let out = render_uncovered(&["qunix-mm".to_string()], &map);
        assert!(out.contains("qunix-mm") && out.contains("src/buddy.rs:7"), "{out:?}");
    }

    #[test]
    fn uncovered_lines_reports_only_zero_hit_lines_with_their_file() {
        let lcov = "\
SF:/w/crates/qunix-sync/src/lib.rs
DA:10,5
DA:11,0
DA:12,0
end_of_record
SF:/home/user/.cargo/registry/src/x/lib.rs
DA:99,0
end_of_record
";
        let uncovered = uncovered_lines(lcov, &root());
        // Covered lines must not appear, or the report is noise.
        assert_eq!(
            uncovered["qunix-sync"],
            vec!["crates/qunix-sync/src/lib.rs:11", "crates/qunix-sync/src/lib.rs:12"]
        );
        // Dependencies are not the contributor's problem.
        assert_eq!(uncovered.len(), 1);
    }

    #[test]
    fn a_rendered_baseline_preserves_per_crate_notes() {
        // The rationale is the thing CONTRIBUTING depends on; a rewrite that
        // drops it destroys the only durable record.
        let previous = "# header\n\n# xtask is low because dispatch only spawns cargo.\nxtask = 31.43\n";
        let notes = existing_notes(previous);
        let mut measured = BTreeMap::new();
        measured.insert("xtask".to_string(), Lines { hit: 40, found: 100 });
        let rendered = render_baseline(&measured, &notes);
        assert!(rendered.contains("dispatch only spawns cargo"), "note dropped: {rendered}");
        assert_eq!(parse_baseline(&rendered)["xtask"], 40.0);
    }

    #[test]
    fn the_file_header_is_not_attributed_to_the_first_crate() {
        let notes = existing_notes("# header line\n\nqunix-mm = 90.00\n");
        assert!(!notes.contains_key("qunix-mm"), "header captured as a crate note");
    }

    #[test]
    fn a_rendered_baseline_round_trips() {
        let mut measured = BTreeMap::new();
        measured.insert("qunix-mm".to_string(), Lines { hit: 9, found: 10 });
        let parsed = parse_baseline(&render_baseline(&measured, &BTreeMap::new()));
        assert_eq!(parsed["qunix-mm"], 90.0);
    }
}
