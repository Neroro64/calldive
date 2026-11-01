mod analysis;
mod diffview;
mod git;
mod syntax;
mod ui;

use analysis::Inputs;
use anyhow::{bail, Result};
use clap::Parser;
use std::collections::{HashMap, HashSet};

/// Interactive, call-graph aware code review for the terminal.
///
/// calldive parses the changed files of a git range with tree-sitter,
/// builds the call graph per revision, and reviews the change the way it
/// executes: starting at entry points, DFS into callees, with a line diff
/// for every hop.
#[derive(Parser, Debug)]
#[command(name = "calldive", version, about)]
struct Args {
    /// base revision (default HEAD~1)
    base: Option<String>,

    /// head revision, or "worktree" (default worktree)
    head: Option<String>,

    /// repository root (default: cwd)
    #[arg(long)]
    repo: Option<String>,

    /// start from this function only (simple name)
    #[arg(short = 'e', long)]
    entry: Option<String>,

    /// print the review tree and exit (no TUI)
    #[arg(long)]
    dump: bool,

    /// show unchanged entry points too
    #[arg(long)]
    all: bool,

    /// include functions inside test modules / test files
    #[arg(long)]
    tests: bool,

    /// start with unchanged branches hidden
    #[arg(long)]
    focus: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cwd = std::env::current_dir()?;
    let repo_path = args
        .repo
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or(cwd);
    let repo = git::GitRepo::open(&repo_path)?;

    let base = args.base.clone().unwrap_or_else(|| "HEAD~1".to_string());
    let head = args
        .head
        .clone()
        .unwrap_or_else(|| git::WORKTREE.to_string());
    if !git::is_worktree(&base) && !repo.ref_exists(&base) {
        bail!(
            "base ref '{}' does not resolve (single-commit repo? pass an explicit base)",
            base
        );
    }
    let base_rev = git::resolve_rev(&repo, &base)?;
    let head_rev = git::resolve_rev(&repo, &head)?;

    let changes = repo.changed_files(&base, &head)?;
    if changes.is_empty() {
        println!("no file changes between {base} and {head}");
        return Ok(());
    }

    // gather sources for changed, reviewable files
    let mut files: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    for ch in &changes {
        if syntax::lang_for_path(&ch.path).is_none() {
            continue;
        }
        let base_src = match &base_rev {
            // added files have no base version
            _ if ch.status == 'A' => None,
            Some(r) => Some(repo.read_file(Some(r), &ch.path).unwrap_or_default()),
            None => None,
        };
        let head_src = match (&head_rev, ch.status) {
            (_, 'D') => None,
            (Some(r), _) => Some(repo.read_file(Some(r), &ch.path).unwrap_or_default()),
            (None, _) => repo.read_file(None, &ch.path).ok(),
        };
        files.push((ch.path.clone(), base_src, head_src));
    }
    if files.is_empty() {
        println!("changed files, but none in a supported language (Rust, Python, TypeScript/TSX, JavaScript/JSX, Go, C#, C, C++)");
        return Ok(());
    }

    // unchanged files: parsed for caller edges into changed symbols only
    let changed_set: HashSet<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    let mut extra_files: Vec<(String, String)> = Vec::new();
    for path in repo.tracked_files(head_rev.as_deref())?.into_iter() {
        if changed_set.contains(path.as_str()) || syntax::lang_for_path(&path).is_none() {
            continue;
        }
        if let Ok(src) = repo.read_file(head_rev.as_deref(), &path) {
            if src.len() <= 512 * 1024 {
                extra_files.push((path, src));
            }
        }
    }

    let inputs = Inputs {
        files,
        extra_head: extra_files,
        include_tests: args.tests,
    };
    let a = analysis::analyze(&inputs);

    // base sources for diffing (path -> src)
    let mut base_srcs: HashMap<String, String> = HashMap::new();
    for (path, base_src, _) in &inputs.files {
        if let Some(src) = base_src {
            base_srcs.insert(path.clone(), src.clone());
        }
    }

    // roots
    let roots: Vec<String> = match &args.entry {
        Some(e) => {
            let mut hit = a
                .nodes
                .keys()
                .filter(|k| k.ends_with(&format!("::{e}")))
                .cloned()
                .collect::<Vec<_>>();
            if hit.len() != 1 {
                bail!("--entry {e}: expected exactly 1 match, got {}", hit.len());
            }
            vec![hit.pop().unwrap()]
        }
        None => a
            .entry_points
            .iter()
            .filter(|k| {
                // default: every entry point whose subtree carries a change
                // (includes unchanged entry points that reach changed code)
                args.all || a.nodes.get(*k).map(|n| n.relevant).unwrap_or(true)
            })
            .cloned()
            .collect(),
    };

    let (ad, rm, ch, mv) = a.summary;
    let head_label = if git::is_worktree(&head) {
        "worktree".to_string()
    } else {
        head.clone()
    };

    if args.dump {
        println!(
            "calldive {base} → {head_label}   fns +{ad} -{rm} ~{ch} >{mv}   entry points: {}",
            roots.len()
        );
        let expanded: HashSet<String> = roots.iter().cloned().collect();
        let rows = analysis::build_rows(&a, &roots, &expanded, false);
        let mut file_counts: HashMap<String, usize> = HashMap::new();
        for n in a.nodes.values() {
            *file_counts.entry(short(&n.file).to_string()).or_default() += 1;
        }
        for r in &rows {
            let n = &a.nodes[&r.key];
            let indent = "  ".repeat(r.depth);
            let kids = if r.has_children && !expanded.contains(&r.key) {
                " …"
            } else {
                ""
            };
            println!(
                "{}{} {} :{}  {}:{}-{}{}",
                indent,
                n.status.glyph(),
                n.name,
                display_file(&n.file, &file_counts),
                n.start_row + 1,
                n.status.glyph(),
                n.end_row,
                kids
            );
        }
        return Ok(());
    }

    // TUI
    let mut app = ui::App::new(&a, roots, args.focus, &base_srcs);
    app.flash = Some(format!("reviewing {base} → {head_label}"));
    ui::run(&mut app)?;
    Ok(())
}

fn short(f: &str) -> &str {
    f.rsplit('/').next().unwrap_or(f)
}

/// Display path for a file: basename, or `parent/base` when basenames
/// collide across the review set (e.g. several crates' lib.rs).
fn display_file(f: &str, counts: &std::collections::HashMap<String, usize>) -> String {
    let base = short(f);
    if counts.get(base).copied().unwrap_or(0) > 1 {
        let mut parts: Vec<&str> = f.split('/').collect();
        let file = parts.pop().unwrap_or("");
        let parent = parts.pop().unwrap_or("");
        format!("{parent}/{file}")
    } else {
        base.to_string()
    }
}
