//! End-to-end tests: real git fixture repos, real binary invocations.
//!
//! Each test builds a scratch repo, runs the compiled binary with `--dump`
//! (non-interactive render of the same model the TUI shows), and asserts
//! on the review tree.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static REPO_SEQ: AtomicUsize = AtomicUsize::new(0);

struct Repo {
    root: PathBuf,
    keep: bool,
}

impl Repo {
    fn new(name: &str) -> Self {
        let n = REPO_SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("calldive-e2e-{}-{}-{n}", std::process::id(), name));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        let r = Self { root, keep: false };
        r.git(&["init", "-q"]);
        r.git(&["config", "user.email", "test@example.com"]);
        r.git(&["config", "user.name", "test"]);
        r
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .expect("git spawn");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(&self, rel: &str, content: &str) {
        fs::write(self.root.join(rel), content).unwrap();
    }

    fn commit_all(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", msg]);
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_calldive"))
            .arg("--repo")
            .arg(&self.root)
            .args(args)
            .output()
            .expect("calldive spawn");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// Base lib.rs + unchanged jobs.rs, committed once.
fn write_base(repo: &Repo) {
    repo.write(
        "src/lib.rs",
        r#"pub fn validate(amount: u32) -> bool { amount > 0 }

pub fn legacy_check(order: &Order) -> bool { order.total < 1000 }

pub fn process(order: &Order) -> &'static str {
    if !validate(order.total) { return "bad"; }
    if legacy_check(order) { return skip(order); }
    charge(order)
}

pub fn skip(order: &Order) -> &'static str { "skipped" }

pub fn charge(order: &Order) -> &'static str { "charged" }

pub struct Order { pub total: u32 }
"#,
    );
    // unchanged caller in another file: process is NOT a true entry point
    repo.write(
        "src/jobs.rs",
        r#"use crate::lib::process;

pub fn retry_all(orders: &[Order]) {
    for o in orders { process(o); }
}
"#,
    );
}

/// Head lib.rs: 1 changed body, 1 rename, 1 removal, 2 additions,
/// and `process` rewired to the new helpers.
fn write_head(repo: &Repo) {
    repo.write(
        "src/lib.rs",
        r#"pub fn validate(amount: u32) -> bool { amount > 0 && amount < 10_000 }

pub fn assess_risk(order: &Order) -> bool { order.total > 500 }

pub fn process(order: &Order) -> &'static str {
    if !validate(order.total) { return "bad"; }
    if assess_risk(order) { return flag_for_review(order); }
    charge_back(order)
}

pub fn flag_for_review(order: &Order) -> &'static str { "flagged" }

pub fn charge_back(order: &Order) -> &'static str { "charged" }

pub struct Order { pub total: u32 }
"#,
    );
}

fn fixture() -> Repo {
    let repo = Repo::new("main");
    write_base(&repo);
    repo.commit_all("base");
    write_head(&repo);
    repo.commit_all("head");
    repo
}

#[test]
fn full_review_tree_statuses_and_structure() {
    let repo = fixture();
    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");

    // summary counts: +2 (assess_risk, flag_for_review) -2 (legacy_check, skip)
    // ~2 (process, validate) >1 (charge→charge_back rename)
    assert!(
        out.contains("fns +2 -2 ~2 >1"),
        "summary line wrong:\n{out}"
    );

    // process is the top root (body changed sorts first) even though
    // retry_all in unchanged jobs.rs calls it
    let process_idx = out.find("~ process").expect("process as changed root");
    let validate_idx = out.find("~ validate").expect("validate under process");
    let assess_idx = out.find("+ assess_risk").expect("assess_risk added");
    let flag_idx = out
        .find("+ flag_for_review")
        .expect("flag_for_review added");
    let charge_back_idx = out.find("> charge_back").expect("charge renamed");
    assert!(
        process_idx < validate_idx,
        "validate must nest under process:\n{out}"
    );
    assert!(process_idx < assess_idx && process_idx < flag_idx);
    assert!(process_idx < charge_back_idx);

    // removed functions are reported as roots too (nothing calls them anymore)
    assert!(out.contains("- legacy_check"), "removed fn missing:\n{out}");
    assert!(out.contains("- skip"), "removed fn missing:\n{out}");

    // unchanged caller file never becomes a node
    assert!(
        !out.contains("retry_all"),
        "jobs.rs leaked into node set:\n{out}"
    );

    // nesting depth: charge_back sits one level under process (2-space indent)
    for line in out.lines() {
        if line.contains("charge_back") {
            assert!(
                line.starts_with("  ") && !line.starts_with("    "),
                "charge_back not nested at depth 1: '{line}'"
            );
        }
        if line.contains("~ process") {
            assert!(!line.starts_with(' '), "process must be a root: '{line}'");
        }
    }
}

#[test]
fn entry_scoping_limits_the_tree() {
    let repo = fixture();
    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "-e", "flag_for_review", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("entry points: 1"), "expected 1 root:\n{out}");
    assert!(out.contains("+ flag_for_review"));
    // leaf: no children lines indented under it
    let flagged_line = out.lines().find(|l| l.contains("flag_for_review")).unwrap();
    assert!(
        !flagged_line.ends_with("…"),
        "must have no children: '{flagged_line}'"
    );
}

#[test]
fn focus_mode_is_a_cli_flag_without_effect_on_dump() {
    // --focus only affects TUI defaults; dump renders the full tree either way
    let repo = fixture();
    let (code, out, _) = repo.run(&["HEAD~1", "HEAD", "--focus", "--dump"]);
    assert_eq!(code, 0);
    assert!(out.contains("~ validate"));
}

#[test]
fn worktree_is_the_default_head() {
    let repo = Repo::new("worktree");
    write_base(&repo);
    repo.commit_all("base");
    write_head(&repo);
    repo.commit_all("head");
    // one more change on top: staged but uncommitted (git diff never sees
    // untracked files, and that is the reviewer's reality too)
    repo.write(
        "src/audit.rs",
        "pub fn audit(order: &Order) -> bool { order.total == 0 }\n",
    );
    repo.git(&["add", "-A"]);
    let (code, out, err) = repo.run(&["--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("HEAD~1 → worktree"),
        "header wrong (default base is HEAD~1):\n{out}"
    );
    assert!(
        out.contains("+ audit"),
        "worktree-only file missing:\n{out}"
    );
    // and the committed changes are still in scope
    assert!(out.contains("~ process"));
}

#[test]
fn unknown_base_ref_fails_with_clear_error() {
    let repo = fixture();
    let (code, _out, err) = repo.run(&["does-not-exist", "HEAD", "--dump"]);
    assert_ne!(code, 0);
    assert!(
        err.contains("does-not-exist") && err.contains("does not resolve"),
        "unclear error: {err}"
    );
}

#[test]
fn single_commit_repo_default_base_fails() {
    let repo = Repo::new("single");
    write_base(&repo);
    repo.commit_all("base");
    // default base HEAD~1 cannot resolve with one commit
    let (code, _out, err) = repo.run(&["--dump"]);
    assert_ne!(code, 0);
    assert!(err.contains("HEAD~1"), "error should name the ref: {err}");
}

#[test]
fn identical_refs_report_no_changes() {
    let repo = fixture();
    let (code, out, err) = repo.run(&["HEAD", "HEAD", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("no file changes"), "unexpected output:\n{out}");
}

#[test]
fn test_functions_hidden_by_default_included_with_tests_flag() {
    let repo = Repo::new("testfilter");
    repo.write(
        "src/lib.rs",
        r#"pub fn validate(amount: u32) -> bool { amount > 0 }

pub fn process(order: &Order) -> bool {
    validate(order.total)
}

pub struct Order { pub total: u32 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_validate_rejects_zero() {
        assert!(!validate(0));
    }
}
"#,
    );
    repo.commit_all("base");
    repo.write(
        "src/lib.rs",
        r#"pub fn validate(amount: u32) -> bool { amount > 0 && amount < 10_000 }

pub fn process(order: &Order) -> bool {
    validate(order.total)
}

pub struct Order { pub total: u32 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_validate_rejects_zero() {
        assert!(!validate(0));
    }
    #[test]
    fn test_validate_rejects_huge() {
        assert!(!validate(20_000));
    }
}
"#,
    );
    repo.commit_all("head");

    // default: test module fns are not nodes; the change is visible via
    // validate alone
    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(!out.contains("test_validate"), "tests leaked:\n{out}");
    assert!(out.contains("~ validate"));

    // --tests brings them in; the new test fn shows as added
    let (code, out, err) = repo.run(&["HEAD~1", "HEAD", "--tests", "--dump"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("+ test_validate_rejects_huge"),
        "missing new test:\n{out}"
    );
}

#[test]
fn all_flag_includes_clean_entry_points() {
    // base and head differ only inside flag_for_review; everything else is
    // unchanged, and unused_helper is a clean, uncalled entry point
    let lib = |flag_return: &str| {
        format!(
            r#"pub fn validate(amount: u32) -> bool {{ amount > 0 && amount < 10_000 }}

pub fn assess_risk(order: &Order) -> bool {{ order.total > 500 }}

pub fn process(order: &Order) -> &'static str {{
    if !validate(order.total) {{ return "bad"; }}
    if assess_risk(order) {{ return flag_for_review(order); }}
    charge_back(order)
}}

pub fn flag_for_review(order: &Order) -> &'static str {{ "{flag_return}" }}

pub fn charge_back(order: &Order) -> &'static str {{ "charged" }}

pub fn unused_helper() -> u32 {{ 42 }}

pub struct Order {{ pub total: u32 }}
"#
        )
    };
    let repo = Repo::new("allflag");
    repo.write("src/lib.rs", &lib("flagged"));
    repo.commit_all("base");
    repo.write("src/lib.rs", &lib("flagged!"));
    repo.commit_all("head");

    // default: only the changed chain's root (process) is listed
    let (code, out, _) = repo.run(&["HEAD~1", "HEAD", "--dump"]);
    assert_eq!(code, 0);
    let default_roots = out
        .lines()
        .find(|l| l.contains("entry points:"))
        .unwrap()
        .to_string();
    // --all adds the clean unused_helper root
    let (code, out_all, _) = repo.run(&["HEAD~1", "HEAD", "--all", "--dump"]);
    assert_eq!(code, 0);
    let all_roots = out_all
        .lines()
        .find(|l| l.contains("entry points:"))
        .unwrap()
        .to_string();
    assert!(
        out_all.contains("unused_helper"),
        "clean root missing:\n{out_all}"
    );

    let n = |s: &str| -> usize { s.rsplit(':').next().unwrap().trim().parse().unwrap_or(0) };
    assert!(n(&default_roots) >= 1);
    assert_eq!(
        n(&default_roots),
        1,
        "default should be 1 root: {default_roots}"
    );
    assert_eq!(
        n(&all_roots),
        2,
        "--all must add the clean root: {all_roots}"
    );
}
