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

/// Crates measured. The same list `xtask test` runs, because a crate that is
/// tested but not ratcheted loses its floor silently and a crate that is
/// ratcheted but not tested measures zero.
use crate::HOST_CRATES as MEASURED;

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

/// Baseline lines that look like a floor but are not readable as one.
///
/// [`parse_baseline`] drops whatever it cannot read. That is the right shape for
/// a parser and precisely the wrong shape for a gate: `qunix-mm = 98,57` — a
/// decimal comma, a merge-conflict marker, a floor moved onto the same line as
/// its note — removes the crate from the baseline *entirely*. `check` then finds
/// no floor for it, reports it as a **new crate**, and prints success for a crate
/// whose ratchet it just stopped enforcing. Nothing else notices, because
/// [`unmeasured_floors`] compares against the parsed baseline and the entry is
/// not in it either.
///
/// It is the same hole as the unmeasured-floor one, a step earlier in the
/// pipeline: there the measurement went missing, here the floor does. Both make
/// the tool confidently green about a crate it is no longer checking, so both
/// are fatal rather than skipped.
fn malformed_baseline_lines(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return false;
            }
            // Exactly what `parse_baseline` accepts. Written as its negation
            // rather than sharing a helper on purpose: if the two ever disagree,
            // the round-trip test below fails, whereas a shared helper would
            // make them agree by construction even when both are wrong.
            let Some((name, value)) = trimmed.split_once('=') else { return true };
            name.trim().trim_matches('"').is_empty() || value.trim().parse::<f64>().is_err()
        })
        .map(|(index, line)| (index + 1, line.to_string()))
        .collect()
}

fn render_malformed(lines: &[(usize, String)]) -> String {
    lines
        .iter()
        .map(|(number, line)| format!("  {BASELINE}:{number}: {line}"))
        .collect::<Vec<_>>()
        .join("\n")
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
/// The generated file header. Written by `render_baseline`, recognised
/// line-for-line by `strip_header`, so the two cannot drift apart.
const HEADER: &str = "\
# Per-crate line coverage floor. A change may raise these or hold them.
#
# Any drop, however small, needs a `#` comment directly above that
# crate's line *beginning* with the new figure and saying which code is
# uncoverable and why; `cargo xtask coverage --update` refuses to write
# one otherwise. The tolerance in xtask/src/coverage.rs applies to
# reading a floor, so a noisy measurement does not fail a build; it
# does not authorise lowering one.
";

/// Drops the leading lines of a comment block that are the file header.
///
/// Each leading line is matched against the lines of [`HEADER`] individually,
/// rather than testing the block's first line and discarding the whole block.
/// That test threw away a note written directly under the header with no blank
/// line between them — the same "file can reach a state no edit fixes" the
/// header rule was introduced to remove, since the ratchet's own error message
/// tells users to write exactly such a note above a crate's line.
///
/// Matched as a set, not in order, so reflowing the header does not turn it into
/// a note. A line that is *not* header text ends the header, and everything from
/// there on is the crate's note.
fn strip_header(pending: &[String]) -> &[String] {
    let header: Vec<&str> = HEADER.lines().map(str::trim).collect();
    let end = pending.iter().position(|l| !header.contains(&l.trim())).unwrap_or(pending.len());
    &pending[end..]
}

fn existing_notes(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut pending: Vec<String> = Vec::new();
    // The file header belongs to no crate, and attributing it to the first one
    // duplicates it on every subsequent render -- which is what happened: the
    // header ended up inside `qunix-hal-x86_64`'s note block. A blank line
    // normally separates them, but relying on that made the rule depend on
    // formatting nobody enforces. Identified by content, and only the lines
    // that are the header: whatever follows them is a note and is kept.
    let mut at_header = true;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            pending.push(line.to_string());
        } else if let Some((name, _)) = trimmed.split_once('=') {
            let note = if at_header { strip_header(&pending) } else { &pending[..] };
            if !note.is_empty() {
                notes.insert(name.trim().trim_matches('"').to_string(), note.to_vec());
            }
            pending.clear();
            at_header = false;
        } else if trimmed.is_empty() {
            at_header = false;
            // A blank line ends a block, so the file header is not attributed
            // to whichever crate happens to be listed first.
            pending.clear();
        }
    }
    notes
}

fn render_baseline(measured: &BTreeMap<String, Lines>, notes: &BTreeMap<String, Vec<String>>) -> String {
    let mut out = String::from(HEADER);
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

/// Rounds to the two decimals the baseline file stores.
///
/// Every comparison against a committed floor has to go through this, because
/// the file is the source of truth and it holds two decimals. Comparing a
/// full-precision measurement against a rounded floor reports a crate that did
/// not move as having dropped.
fn round2(percent: f64) -> f64 {
    (percent * 100.0).round() / 100.0
}

/// Crates whose floor `--update` would lower without a stated reason.
///
/// Three things this checks that an earlier version did not, each of which let
/// a floor slide:
///
/// * **Any** lowering counts, not only one larger than [`TOLERANCE_PP`]. The
///   tolerance exists so a *read* of a noisy measurement does not fail the
///   build; applying it to the *write* let every floor walk down 0.5 pp per
///   `--update`, indefinitely.
/// * A reason must mention the new figure. Requiring merely that some comment
///   exists is satisfied forever by a comment written years earlier for a
///   different number, which is what "documented" degenerated to.
/// * The comparison is against the committed floor, so a crate with no floor
///   yet (a new crate) is never treated as a lowering.
fn lowering_without_reason(
    old: &BTreeMap<String, f64>,
    measured: &BTreeMap<String, Lines>,
    notes: &BTreeMap<String, Vec<String>>,
) -> Vec<(String, f64, f64)> {
    measured
        .iter()
        .filter(|(name, lines)| {
            let Some(&floor) = old.get(*name) else { return false };
            // Compared at the precision the file stores, not at full precision.
            // The baseline holds two decimals, so 97.3262 is written as 97.33
            // and then reads back as *higher* than itself -- which made an
            // unchanged crate look like a lowering and blocked every update.
            if round2(lines.percent()) >= round2(floor) {
                return false;
            }
            // The reason has to name the number it is justifying. Without
            // that, a stale note keeps authorising every future drop.
            // The figure must *head* the note, not merely appear in it. A
            // substring match let `# 97.335 ...` authorise a drop to 97.33, and
            // the house style of recording a transition (`# 25.00 -> 24.90`)
            // meant the note kept authorising a return to the old value forever.
            let figure = format!("{:.2}", round2(lines.percent()));
            !notes.get(*name).and_then(|note| note.first()).is_some_and(|first| {
                let rest = first.trim_start_matches('#').trim_start();
                // The figure must be followed by something that is not another
                // digit, or `97.335` still satisfies a required `97.33`.
                rest.strip_prefix(&figure)
                    .is_some_and(|tail| !tail.starts_with(|c: char| c.is_ascii_digit()))
            })
        })
        .map(|(name, lines)| (name.clone(), round2(lines.percent()), old[name]))
        .collect()
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

/// How a measurement stands against its committed floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Regressed,
    Improved,
    Held,
}

/// Compares one measurement against one floor.
///
/// This is the ratchet — the single decision the whole command exists to make —
/// and it was written inline in `check`, downstream of `cargo llvm-cov`, so
/// nothing could test it. Every other rule here had a negative test and this one
/// had none in either direction: no test asserted that a real drop is *caught*.
/// Inverting the comparison, or applying the tolerance to both sides, would have
/// left every existing test passing.
fn verdict(now: f64, floor: f64) -> Verdict {
    if now + TOLERANCE_PP < floor {
        Verdict::Regressed
    } else if now > floor + TOLERANCE_PP {
        Verdict::Improved
    } else {
        Verdict::Held
    }
}

/// Crates that carry a floor but produced no measurement.
///
/// The comparison loop walks what was measured, so a crate missing from the
/// report is never compared against its floor at all — the ratchet prints
/// success for a crate it did not look at. The genuine triggers are a crate
/// dropped from `MEASURED` (or from the workspace) while its baseline line
/// stays, a rename, and a crate that emits no `SF:` records at all. A crate
/// whose *tests fail to link* is not one of them: `measure` bails on a non-zero
/// `cargo llvm-cov` exit long before this runs. Split out from `check` so the
/// condition can be tested without running `cargo llvm-cov`.
fn unmeasured_floors(
    baseline: &BTreeMap<String, f64>,
    measured: &BTreeMap<String, Lines>,
) -> Vec<(String, f64)> {
    baseline
        .iter()
        .filter(|(name, _)| !measured.contains_key(*name))
        .map(|(name, floor)| (name.clone(), *floor))
        .collect()
}

fn render_unmeasured(unmeasured: &[(String, f64)]) -> String {
    unmeasured
        .iter()
        .map(|(name, floor)| format!("  {name:<20} floor {floor:.2}%  (no coverage reported)"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Crate names the caller explicitly authorised a floor to be deleted for.
///
/// `--update` rewrites the baseline from what was measured, so a crate that has
/// vanished from the report loses its floor *and* the recorded reason for it,
/// silently. The read-only path's error message used to recommend `--update` as
/// the way to clear such a line, which made "the crate disappeared for a bad
/// reason" fixable by the command the ratchet itself suggested — the same shape
/// as widening the tolerance to make a red build green. Deleting a floor now
/// takes naming the crate.
///
/// Read from the process arguments rather than added to `check`'s signature
/// because the dispatch in `main` collapses the command line to a single
/// `update` flag; the parsing is a pure function so the rule is testable without
/// running a process. Only the `--drop-crate=<name>` form is accepted, so a
/// crate name can never be swallowed from an adjacent argument.
fn dropped_crates(args: &[String]) -> Vec<String> {
    args.iter()
        .filter_map(|a| a.strip_prefix("--drop-crate="))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Fails if any baseline line is neither a comment, a blank, nor a readable floor.
fn reject_malformed(text: &str) -> Result<()> {
    let malformed = malformed_baseline_lines(text);
    if !malformed.is_empty() {
        bail!(
            "{n} line(s) in {BASELINE} are not readable as a floor:\n{list}\n\n\
             A line the parser cannot read is silently dropped, which removes that \
             crate's floor entirely -- the ratchet then treats it as a new crate and \
             reports success for something it has stopped enforcing. Fix the line or \
             comment it out with `#`.",
            n = malformed.len(),
            list = render_malformed(&malformed)
        );
    }
    Ok(())
}

/// Fails when a crate carries a floor that this run did not measure.
///
/// Extracted from `check` so the refusal itself is testable. It was a `bail!`
/// inline in a function that needs a real `cargo llvm-cov` run to reach, so
/// nothing exercised it: deleting the whole block failed no test, and the gate
/// that exists to stop a floor being silently unenforced was itself silently
/// unenforced. That is the same shape as the licensing `verdict` extraction --
/// a rule computed and then not acted on.
///
/// What remains untestable is the call site: a test cannot prove `check` still
/// calls this. That limit is irreducible without an integration harness, and it
/// is the reason the call is a bare `?` on its own line rather than something
/// easy to drop during an edit.
fn gate_unmeasured(
    baseline: &BTreeMap<String, f64>,
    measured: &BTreeMap<String, Lines>,
) -> Result<()> {
    let unmeasured = unmeasured_floors(baseline, measured);
    if unmeasured.is_empty() {
        return Ok(());
    }
    bail!(
        "{n} crate(s) have a coverage floor but produced no measurement:\n{list}\n\n\
         A floor that is never measured is not enforced. Restore the crate: put it \
         back in `MEASURED` and in the workspace, or fix whatever stopped it \
         emitting coverage records. Only if it is genuinely gone, delete its floor \
         deliberately with `cargo xtask coverage --update {flags}` -- which also \
         deletes the recorded reason for that floor.",
        n = unmeasured.len(),
        list = render_unmeasured(&unmeasured),
        flags = unmeasured
            .iter()
            .map(|(name, _)| format!("--drop-crate={name}"))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

/// Measures coverage and compares it against the committed floor.
pub fn check(root: &Path, update: bool) -> Result<()> {
    let (measured, lcov) = measure(root)?;
    let baseline_path = root.join(BASELINE);

    if update {
        // Read before write so per-crate rationales survive the rewrite.
        let previous = std::fs::read_to_string(&baseline_path).unwrap_or_default();
        // Before anything reads a floor out of it. An unreadable line is a floor
        // this run cannot see, so `--update` would drop the crate and its note
        // without either gate below noticing there was anything to authorise.
        reject_malformed(&previous)?;
        let notes = existing_notes(&previous);
        // A floor may only go *down* with a recorded reason. Enforced rather
        // than requested: the last time this was a convention, a floor was
        // lowered on a justification that turned out to be false, and nothing
        // caught it.
        let old = parse_baseline(&previous);
        // The rewrite renders only what was measured, so a floor with no
        // measurement disappears together with its note. That is exactly the
        // deletion the read-only path refuses, so it is refused here too --
        // otherwise the escape hatch the ratchet advertises is also the way
        // round it. Naming the crate is the opt-in.
        let unmeasured = unmeasured_floors(&old, &measured);
        let authorised = dropped_crates(&std::env::args().collect::<Vec<_>>());
        let unauthorised: Vec<(String, f64)> = unmeasured
            .iter()
            .filter(|(name, _)| !authorised.contains(name))
            .cloned()
            .collect();
        if !unauthorised.is_empty() {
            bail!(
                "refusing to delete {n} floor(s) that produced no measurement:\n{list}\n\n\
                 A crate usually stops being measured because something broke: it left \
                 the workspace, it was renamed, or it emits no coverage records. Restore \
                 it, or if it is genuinely gone, say so:\n  \
                 cargo xtask coverage --update {flags}\n\
                 Deleting the line also deletes the recorded reason for its floor.",
                n = unauthorised.len(),
                list = render_unmeasured(&unauthorised),
                flags = unauthorised
                    .iter()
                    .map(|(name, _)| format!("--drop-crate={name}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        let undocumented = lowering_without_reason(&old, &measured, &notes);
        if !undocumented.is_empty() {
            // The figures are in the message because the read-only path prints
            // nothing for a drop inside TOLERANCE_PP, so a user blocked here
            // would otherwise have to run `cargo llvm-cov` by hand to discover
            // the number they are required to write down.
            let mut detail = String::new();
            for (name, now, floor) in &undocumented {
                detail.push_str(&format!(
                    "\n  {name}: {now:.2}% (floor {floor:.2}%) -- the comment above \
                     `{name}` in {BASELINE} must begin `# {now:.2}`"
                ));
            }
            bail!(
                "refusing to lower a floor without a recorded reason:{detail}\n\n\
                 The reason has to start with the new figure, so a note written for \
                 an earlier drop cannot keep authorising later ones."
            );
        }
        // A note whose leading figure no longer matches is reported even when
        // the floor *rose*. The gate above only fires on a lowering, so
        // without this a note silently describes a number the file stopped
        // holding -- which is how `qunix-sync` came to claim 97.33 beside a
        // floor of 98.54.
        for (name, lines) in &measured {
            let figure = format!("{:.2}", round2(lines.percent()));
            let heads_with_figure = notes
                .get(name)
                .and_then(|note| note.first())
                .is_some_and(|first| {
                    let rest = first.trim_start_matches('#').trim_start();
                    rest.strip_prefix(&figure)
                        .is_some_and(|tail| !tail.starts_with(|c: char| c.is_ascii_digit()))
                });
            if notes.contains_key(name) && !heads_with_figure {
                println!(
                    "coverage: the note above `{name}` does not begin `# {figure}`; \
                     it now describes a figure the file no longer holds"
                );
            }
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
    reject_malformed(&baseline_text)?;
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
        let line = format!("  {name:<20} {now:>6.2}%  floor {floor:.2}%  ({:+.2} pp)", now - floor);
        match verdict(now, floor) {
            Verdict::Regressed => {
                regressions.push(line);
                regressed_names.push(name.clone());
            }
            Verdict::Improved => improvements.push(line),
            Verdict::Held => {}
        }
    }

    // The loop above walks what was *measured*, so a crate that has a floor but
    // produced no measurement is never compared against it. That is not a
    // hypothetical gap: a crate dropped from `MEASURED` or from the workspace,
    // renamed, or emitting no records stops appearing in the report and its
    // floor silently stops being enforced -- the ratchet reports success for a
    // crate it did not look at. Named explicitly, so removing a crate means
    // editing the baseline deliberately.
    gate_unmeasured(&baseline, &measured)?;

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

    /// A crate under `crates/` that is not in `MEASURED` is not ratcheted, and
    /// nothing says so -- the run prints success for the crates it did measure
    /// and never mentions the one it skipped. `qunix-virtio` sat outside the
    /// ratchet for a whole milestone that way.
    ///
    /// Membership is the directory, not a `std` feature. The first version of
    /// this used the feature as the signal for "host-testable" and passed
    /// `qunix-abi`, which is unconditionally `#![no_std]`, has no such feature,
    /// and runs three host tests -- reproducing, inside the check written to
    /// close the gap, exactly the gap it was written for.
    ///
    /// A crate that genuinely cannot be measured on the host gets no exception
    /// here. It goes in `MEASURED` with a low floor and a note saying which
    /// lines are unreachable and why, as `qunix-hal-x86_64` does.
    #[test]
    fn every_crate_in_the_workspace_is_measured() {
        let root = crate::workspace_root();
        for entry in std::fs::read_dir(root.join("crates")).unwrap() {
            let dir = entry.unwrap().path();
            // A stray file or editor directory under `crates/` is not a crate,
            // and failing on it would turn unrelated filesystem state into a
            // test failure.
            let Ok(manifest) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
                continue;
            };
            // The package name, not the directory name: cargo dispatches `-p`
            // on the former, so a crate whose two names differ would be
            // measured under a name no invocation ever uses.
            let name = manifest
                .lines()
                .find_map(|line| line.trim().strip_prefix("name = "))
                .expect("a crate manifest with no package name")
                .trim()
                .trim_matches('"')
                .to_string();
            assert!(
                MEASURED.iter().any(|(measured, _)| *measured == name),
                "{name} is a workspace crate but is not in MEASURED, so nothing ratchets it"
            );
        }
    }
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

    fn floors(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(n, v)| ((*n).to_string(), *v)).collect()
    }

    fn measured(pairs: &[(&str, u64, u64)]) -> BTreeMap<String, Lines> {
        pairs.iter().map(|(n, hit, found)| ((*n).to_string(), Lines { hit: *hit, found: *found })).collect()
    }

    #[test]
    fn a_measurement_that_rounds_to_its_floor_is_not_a_lowering() {
        // 9733/10000 is 97.33 exactly; 730/750 is 97.3333..., which is written
        // as 97.33 and must not then read back as a drop against itself.
        let old = floors(&[("qunix-sync", 97.33)]);
        let now = measured(&[("qunix-sync", 730, 750)]);
        assert!(
            lowering_without_reason(&old, &now, &BTreeMap::new()).is_empty(),
            "a crate that did not move was reported as lowering its floor"
        );
    }

    #[test]
    fn a_note_naming_a_longer_number_does_not_authorise_a_shorter_one() {
        // `contains` let `97.335` authorise a drop to `97.33`.
        let old = floors(&[("qunix-sync", 97.84)]);
        let now = measured(&[("qunix-sync", 9733, 10000)]);
        let mut notes = BTreeMap::new();
        notes.insert("qunix-sync".to_string(), vec!["# 97.335 earlier".to_string()]);
        assert!(!lowering_without_reason(&old, &now, &notes).is_empty());
    }

    #[test]
    fn a_transition_note_does_not_authorise_a_return_to_the_old_value() {
        // The house style records `OLD -> NEW`. Matching anywhere in the line
        // meant such a note authorised a later drop back to OLD forever.
        let old = floors(&[("qunix-hal-x86_64", 25.00)]);
        let now = measured(&[("qunix-hal-x86_64", 2490, 10000)]);
        let mut notes = BTreeMap::new();
        notes.insert(
            "qunix-hal-x86_64".to_string(),
            vec!["# 25.00 -> 24.90 when the loader landed".to_string()],
        );
        assert!(
            !lowering_without_reason(&old, &now, &notes).is_empty(),
            "a note headed by the old figure authorised a drop to a new one"
        );
    }

    #[test]
    fn a_one_cent_drop_is_not_rounded_away() {
        let old = floors(&[("c", 97.33)]);
        let now = measured(&[("c", 9732, 10000)]);
        assert!(!lowering_without_reason(&old, &now, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_note_above_the_first_crate_is_kept() {
        // The `at_header` rule used to discard it, and the gate's own error
        // message instructs users to write exactly this.
        let notes = existing_notes("# 100.00 because it is pure logic\nqunix-elf = 100.00\n");
        assert!(notes.contains_key("qunix-elf"), "a first-crate note was discarded");
    }

    #[test]
    fn a_stale_note_does_not_authorise_a_new_lowering() {
        // The defect this replaced: the gate asked only whether *some* comment
        // existed, so one written for an earlier drop kept authorising every
        // later one, forever.
        let old = floors(&[("qunix-mm", 90.0)]);
        let now = measured(&[("qunix-mm", 80, 100)]);
        let mut notes = BTreeMap::new();
        notes.insert("qunix-mm".to_string(), vec!["# dropped to 85.00 when X landed".to_string()]);
        assert_eq!(
            lowering_without_reason(&old, &now, &notes).iter().map(|(n, _, _)| n.clone()).collect::<Vec<_>>(),
            vec!["qunix-mm".to_string()],
            "a note naming 85.00 authorised a drop to 80.00"
        );
    }

    #[test]
    fn a_note_naming_the_new_figure_authorises_the_lowering() {
        let old = floors(&[("qunix-mm", 90.0)]);
        let now = measured(&[("qunix-mm", 80, 100)]);
        let mut notes = BTreeMap::new();
        notes.insert("qunix-mm".to_string(), vec!["# 80.00 because the codegen path is host-unreachable".to_string()]);
        assert!(lowering_without_reason(&old, &now, &notes).is_empty());
    }

    #[test]
    fn a_sub_tolerance_drop_still_needs_a_reason() {
        // TOLERANCE_PP exists so a noisy *read* does not fail the build. Applying
        // it to the *write* let every floor walk down 0.5 pp per --update run.
        let old = floors(&[("qunix-sync", 97.84)]);
        let now = measured(&[("qunix-sync", 9750, 10000)]); // 97.50, a 0.34 pp drop
        assert_eq!(
            lowering_without_reason(&old, &now, &BTreeMap::new()).iter().map(|(n, _, _)| n.clone()).collect::<Vec<_>>(),
            vec!["qunix-sync".to_string()],
            "a drop inside the tolerance was written without a reason"
        );
    }

    #[test]
    fn the_unmeasured_gate_actually_refuses() {
        // `a_floor_with_no_measurement_is_reported` covers the computation.
        // This covers the *refusal*, which was previously a `bail!` inline in
        // `check` -- unreachable without a real `cargo llvm-cov` run, so
        // deleting the entire block failed no test. The gate that stops a floor
        // going silently unenforced was itself silently unenforced.
        let baseline = BTreeMap::from([
            ("qunix-mm".to_string(), 98.11),
            ("qunix-gone".to_string(), 91.00),
        ]);
        let measured = BTreeMap::from([("qunix-mm".to_string(), Lines { hit: 9, found: 10 })]);

        let err = gate_unmeasured(&baseline, &measured).unwrap_err().to_string();
        assert!(err.contains("qunix-gone"), "the error does not name the crate: {err}");
        assert!(
            err.contains("--drop-crate=qunix-gone"),
            "the error does not say how to resolve it deliberately: {err}"
        );
        // And the accepting direction, so the gate cannot be made to fire
        // always -- which would be just as broken and would pass the above.
        let complete = BTreeMap::from([("qunix-mm".to_string(), 98.11)]);
        assert!(gate_unmeasured(&complete, &measured).is_ok());
    }

    #[test]
    fn a_floor_with_no_measurement_is_reported() {
        // The silent case the whole check exists for: a crate whose tests stop
        // linking disappears from the report, and every remaining crate passes,
        // so the ratchet says "all crates at or above their floor" having
        // skipped one entirely.
        let baseline = BTreeMap::from([
            ("qunix-mm".to_string(), 98.11),
            ("qunix-gone".to_string(), 91.00),
        ]);
        let measured = BTreeMap::from([("qunix-mm".to_string(), Lines { hit: 9, found: 10 })]);

        let missing = unmeasured_floors(&baseline, &measured);
        assert_eq!(missing, vec![("qunix-gone".to_string(), 91.00)], "got {missing:?}");
        // The floor has to reach the message, or the user cannot tell what is
        // being enforced from the failure alone.
        let rendered = render_unmeasured(&missing);
        assert!(rendered.contains("qunix-gone") && rendered.contains("91.00"), "{rendered}");
    }

    #[test]
    fn every_floor_measured_reports_nothing() {
        let baseline = BTreeMap::from([("qunix-mm".to_string(), 98.11)]);
        let measured = BTreeMap::from([
            ("qunix-mm".to_string(), Lines { hit: 9, found: 10 }),
            // A crate measured without a floor is the new-crate case, handled
            // elsewhere; it must not be mistaken for a missing measurement.
            ("qunix-new".to_string(), Lines { hit: 1, found: 1 }),
        ]);
        assert!(unmeasured_floors(&baseline, &measured).is_empty());
    }

    #[test]
    fn a_crate_with_no_floor_yet_is_not_a_lowering() {
        let now = measured(&[("qunix-elf", 100, 100)]);
        assert!(lowering_without_reason(&BTreeMap::new(), &now, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_rise_is_never_a_lowering() {
        let old = floors(&[("qunix-mm", 90.0)]);
        let now = measured(&[("qunix-mm", 96, 100)]);
        assert!(lowering_without_reason(&old, &now, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn the_generated_header_is_not_captured_as_a_note() {
        // How the header came to be duplicated inside a crate's note block. It
        // is now recognised by its own text rather than by "the block before the
        // first blank line", which was indistinguishable from a legitimate note
        // above the first crate. Written from `HEADER` itself so an edit to the
        // rendered header cannot leave this test asserting about old text.
        let notes = existing_notes(&format!("{HEADER}qunix-mm = 90.00\n"));
        assert!(
            !notes.contains_key("qunix-mm"),
            "the file header was attributed to the first crate: {notes:?}"
        );
    }

    #[test]
    fn a_note_written_directly_under_the_header_survives() {
        // No blank line between them, which is what a user editing the file by
        // hand produces. The old rule keyed off the block's first line and
        // discarded header and note together, so the reason the ratchet demands
        // was silently deleted and could not be re-added.
        let text = format!("{HEADER}# 90.00 because the dispatch arm only spawns cargo\nxtask = 90.00\n");
        let notes = existing_notes(&text);
        assert_eq!(
            notes.get("xtask").map(Vec::as_slice),
            Some(&["# 90.00 because the dispatch arm only spawns cargo".to_string()][..]),
            "a note under the header was lost, or the header came with it: {notes:?}"
        );
        // And the round trip must not duplicate the header inside the note.
        let mut measured = BTreeMap::new();
        measured.insert("xtask".to_string(), Lines { hit: 90, found: 100 });
        let rendered = render_baseline(&measured, &notes);
        assert_eq!(
            rendered.matches("Per-crate line coverage floor").count(),
            1,
            "the header was duplicated into a note: {rendered}"
        );
    }

    #[test]
    fn deleting_a_floor_takes_naming_the_crate() {
        // `--update` renders only what was measured, so an unmeasured floor
        // vanishes along with its recorded reason. The read-only path refuses
        // that; the escape hatch it recommends must not be a way round it.
        assert!(dropped_crates(&["--update".to_string()]).is_empty());
        assert_eq!(
            dropped_crates(&["--update".to_string(), "--drop-crate=qunix-gone".to_string()]),
            vec!["qunix-gone".to_string()]
        );
        // The separated form is not accepted, so a following argument can never
        // be swallowed as a crate name.
        assert!(
            dropped_crates(&["--drop-crate".to_string(), "qunix-gone".to_string()]).is_empty()
        );
        assert!(dropped_crates(&["--drop-crate=".to_string()]).is_empty());
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
    fn the_ratchet_catches_a_real_drop() {
        // The decision the entire command exists to make, and the one rule here
        // that had no test in either direction. Inverting the comparison in
        // `check` would have left every other test in this file passing.
        assert_eq!(verdict(80.0, 90.0), Verdict::Regressed);
        assert_eq!(verdict(0.0, 25.93), Verdict::Regressed);
        // Just past the tolerance is still a regression: the tolerance absorbs
        // measurement drift, it is not an allowance to spend.
        assert_eq!(verdict(90.0 - TOLERANCE_PP - 0.01, 90.0), Verdict::Regressed);
    }

    #[test]
    fn a_drop_within_the_tolerance_holds_and_a_rise_beyond_it_improves() {
        assert_eq!(verdict(90.0, 90.0), Verdict::Held);
        assert_eq!(verdict(90.0 - TOLERANCE_PP, 90.0), Verdict::Held);
        // Inside the tolerance upward is not an improvement either, or the tool
        // would tell contributors to raise a floor on pure noise.
        assert_eq!(verdict(90.0 + TOLERANCE_PP, 90.0), Verdict::Held);
        assert_eq!(verdict(96.0, 90.0), Verdict::Improved);
    }

    #[test]
    fn the_tolerance_is_the_figure_it_claims_to_be() {
        // Pinned deliberately. Widening this is the documented wrong answer to a
        // red build -- it weakens the ratchet for every crate to accommodate
        // one, and it is a one-character edit no reviewer would necessarily
        // question. `qunix-sync` measured 97.84 here and 96.76 on a runner for
        // the same commit; the fix was the test, not this number. Raising it
        // must mean deleting this assertion, which is a decision to argue for.
        assert_eq!(TOLERANCE_PP, 0.5);
        // And it must stay smaller than the smallest drop worth catching. At
        // 1 pp, an entire uncovered function in a 100-line crate passes.
        assert!(TOLERANCE_PP < 1.0, "a tolerance of {TOLERANCE_PP} pp hides a whole function");
    }

    #[test]
    fn an_unreadable_floor_line_is_fatal_rather_than_dropped() {
        // The fail-open this replaced: `parse_baseline` filters out whatever it
        // cannot read, so a typo'd figure deleted the crate's floor, `check` saw
        // no floor and called it a new crate, and the ratchet printed success
        // for a crate it had stopped enforcing.
        let decimal_comma = "qunix-mm = 98,57\n";
        assert!(parse_baseline(decimal_comma).is_empty(), "the premise changed");
        assert_eq!(malformed_baseline_lines(decimal_comma).len(), 1);
        assert!(reject_malformed(decimal_comma).is_err());

        // Every other shape that silently vanishes.
        for text in [
            "qunix-mm 98.57\n",              // the `=` lost to a reflow
            "qunix-mm = 98.5.7\n",           // a stray keystroke
            "qunix-mm = ninety\n",           // a placeholder nobody replaced
            "<<<<<<< HEAD\n",                // an unresolved merge
            "= 98.57\n",                     // the crate name lost
            "qunix-mm = 98.57 # inline\n",   // a note moved onto the floor line
        ] {
            assert!(
                reject_malformed(text).is_err(),
                "a line that parses to nothing was accepted: {text:?}"
            );
        }
    }

    #[test]
    fn a_well_formed_baseline_has_no_malformed_lines() {
        // The other direction, or the check above is satisfied by rejecting
        // everything. Includes the forms the file actually uses: a quoted name,
        // an integer floor, comments and blanks.
        let text = "# a note\n\n\"qunix-mm\" = 98.57\nxtask = 54\n#\n";
        assert!(malformed_baseline_lines(text).is_empty(), "{:?}", malformed_baseline_lines(text));
        assert_eq!(parse_baseline(text).len(), 2);
    }

    #[test]
    fn the_malformed_report_names_the_line_and_its_number() {
        // A gate whose message does not say where to look sends the user back
        // to guessing, which is the failure the uncovered-line report fixed.
        let out = render_malformed(&malformed_baseline_lines("ok = 1.0\nbroken\n"));
        assert!(out.contains(":2:") && out.contains("broken"), "{out}");
    }

    /// The committed baseline read through the tool's own rules.
    ///
    /// It has violated them before -- `qunix-sync` carried a note headed 97.33
    /// beside a floor of 98.54, so the recorded reason described a number the
    /// file no longer held. Checked here rather than only when someone happens
    /// to run `--update`, because the note is the durable record CONTRIBUTING
    /// depends on and a wrong one is worse than none.
    fn committed_baseline() -> String {
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../coverage-baseline.toml"))
            .expect("reading the committed coverage-baseline.toml")
    }

    #[test]
    fn the_committed_baseline_is_readable_by_its_own_parser() {
        let text = committed_baseline();
        reject_malformed(&text).expect("the committed baseline has an unreadable line");
        assert!(!parse_baseline(&text).is_empty());
    }

    #[test]
    fn every_measured_crate_has_a_committed_floor() {
        // A crate added to MEASURED without a floor is reported as "new" forever
        // and is never ratcheted -- it can fall from 100% to 0% and pass.
        let baseline = parse_baseline(&committed_baseline());
        for (name, _) in MEASURED {
            assert!(baseline.contains_key(*name), "{name} is measured but has no floor");
        }
    }

    #[test]
    fn every_committed_floor_names_a_measured_crate() {
        // The direction `unmeasured_floors` catches at runtime, asserted at test
        // time so it does not take a full `cargo llvm-cov` run to notice.
        let baseline = parse_baseline(&committed_baseline());
        for name in baseline.keys() {
            assert!(
                MEASURED.iter().any(|(m, _)| m == name),
                "{name} has a floor but is not in MEASURED, so it is never enforced"
            );
        }
    }

    #[test]
    fn every_committed_note_heads_with_the_floor_it_describes() {
        // The rule `--update` enforces on a lowering, applied to the file as it
        // stands. A note whose figure has drifted authorises nothing and
        // misdescribes everything.
        let text = committed_baseline();
        let baseline = parse_baseline(&text);
        for (name, note) in existing_notes(&text) {
            let floor = baseline.get(&name).unwrap_or_else(|| panic!("note for unknown crate {name}"));
            let figure = format!("{floor:.2}");
            let first = note.first().expect("an empty note block");
            let rest = first.trim_start_matches('#').trim_start();
            assert!(
                rest.strip_prefix(&figure)
                    .is_some_and(|tail| !tail.starts_with(|c: char| c.is_ascii_digit())),
                "the note above `{name}` is headed {first:?} but the floor is {figure}"
            );
        }
    }

    #[test]
    fn a_rendered_baseline_round_trips() {
        let mut measured = BTreeMap::new();
        measured.insert("qunix-mm".to_string(), Lines { hit: 9, found: 10 });
        let parsed = parse_baseline(&render_baseline(&measured, &BTreeMap::new()));
        assert_eq!(parsed["qunix-mm"], 90.0);
    }
}
