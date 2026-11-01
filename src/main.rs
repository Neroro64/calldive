mod analysis;
mod diffview;
mod git;
mod syntax;
mod ui;

use analysis::Inputs;
use anyhow::{bail, Context, Result};
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
    if git::is_worktree(&base) {
        bail!("base ref '{base}' cannot be worktree; pass a commit revision as base");
    }
    let base_rev = repo.resolve_commit(&base).with_context(|| {
        format!("base ref '{base}' does not resolve (single-commit repo? pass an explicit base)")
    })?;
    let head_rev = git::resolve_rev(&repo, &head)?;

    let changes = repo.changed_files(&base_rev, head_rev.as_deref())?;
    if changes.is_empty() {
        println!("no file changes between {base} and {head}");
        return Ok(());
    }

    let (files, extra_files) = gather_sources(
        &repo,
        &changes,
        &base_rev,
        head_rev.as_deref(),
        &base,
        &head,
    )?;
    if files.is_empty() {
        println!("changed files, but none in a supported language (Rust, Python, TypeScript/TSX, JavaScript/JSX, Go, C#, C, C++)");
        return Ok(());
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

type ChangedSources = Vec<(String, Option<String>, Option<String>)>;
const MAX_EXTRA_BYTES: usize = 512 * 1024;

/// Load changed blobs first, then retain only unchanged files mentioning their
/// function names. Both phases share a single git cat-file process.
fn gather_sources(
    repo: &git::GitRepo,
    changes: &[git::FileChange],
    base_sha: &str,
    head_sha: Option<&str>,
    base_label: &str,
    head_label: &str,
) -> Result<(ChangedSources, Vec<(String, String)>)> {
    let reviewable: Vec<&git::FileChange> = changes
        .iter()
        .filter(|change| syntax::lang_for_path(&change.path).is_some())
        .collect();
    if reviewable.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let base_blobs = repo.blobs_at(base_sha)?;
    let base_index: HashMap<&str, &git::Blob> = base_blobs
        .iter()
        .map(|blob| (blob.path.as_str(), blob))
        .collect();
    let head_blobs = match head_sha {
        Some(sha) => Some(repo.blobs_at(sha)?),
        None => None,
    };
    let head_index = head_blobs.as_ref().map(|blobs| {
        blobs
            .iter()
            .map(|blob| (blob.path.as_str(), blob))
            .collect::<HashMap<_, _>>()
    });
    let mut files: ChangedSources = Vec::with_capacity(reviewable.len());
    // (file index, base side, object ID); object IDs also handle paths with newlines.
    let mut requests: Vec<(usize, bool, &str)> = Vec::new();
    for (index, change) in reviewable.iter().enumerate() {
        let base_path = change.old_path.as_deref().unwrap_or(&change.path);
        if change.status != 'A' {
            let blob = base_index.get(base_path).with_context(|| {
                format!(
                    "missing base source '{base_path}' at rev '{base_label}' (review path '{}')",
                    change.path
                )
            })?;
            requests.push((index, true, &blob.oid));
        }
        let mut head_source = None;
        if change.status != 'D' {
            if let Some(blobs) = &head_index {
                let blob = blobs.get(change.path.as_str()).with_context(|| {
                    format!(
                        "missing head source '{}' at rev '{head_label}'",
                        change.path
                    )
                })?;
                requests.push((index, false, &blob.oid));
            } else {
                head_source = Some(repo.read_worktree(&change.path).with_context(|| {
                    format!(
                        "reading head source '{}' at rev '{head_label}'",
                        change.path
                    )
                })?);
            }
        }
        files.push((change.path.clone(), None, head_source));
    }
    let changed_set: HashSet<&str> = changes.iter().map(|change| change.path.as_str()).collect();
    let mut extra_files = Vec::new();
    repo.with_blobs(|reader| {
        let oids: Vec<&str> = requests.iter().map(|request| request.2).collect();
        reader.read(&oids, |index, source| {
            let (file_index, is_base, oid) = requests[index];
            let change = reviewable[file_index];
            let (side, path, rev) = if is_base {
                (
                    "base",
                    change.old_path.as_deref().unwrap_or(&change.path),
                    base_label,
                )
            } else {
                ("head", change.path.as_str(), head_label)
            };
            let source = source.with_context(|| {
                format!("missing {side} source '{path}' at rev '{rev}' (blob {oid})")
            })?;
            if is_base {
                files[file_index].1 = Some(source);
            } else {
                files[file_index].2 = Some(source);
            }
            Ok(())
        })?;
        let Some(search) = changed_name_search(&files)? else {
            return Ok(());
        };
        if let Some(blobs) = &head_blobs {
            let mut candidates: Vec<&git::Blob> = blobs
                .iter()
                .filter(|blob| {
                    !changed_set.contains(blob.path.as_str())
                        && syntax::lang_for_path(&blob.path).is_some()
                        && blob.size <= MAX_EXTRA_BYTES
                })
                .collect();
            candidates.sort_by(|a, b| a.path.cmp(&b.path));
            let oids: Vec<&str> = candidates.iter().map(|blob| blob.oid.as_str()).collect();
            reader.read(&oids, |index, source| {
                let path = &candidates[index].path;
                let source = source.with_context(|| {
                    format!("missing unchanged source '{path}' at rev '{head_label}'")
                })?;
                if source.len() <= MAX_EXTRA_BYTES && contains_identifier(&search, &source) {
                    extra_files.push((path.clone(), source));
                }
                Ok(())
            })?;
        } else {
            for path in repo.tracked_files()? {
                if changed_set.contains(path.as_str()) || syntax::lang_for_path(&path).is_none() {
                    continue;
                }
                let metadata = match std::fs::metadata(repo.root.join(&path)) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("reading worktree metadata '{path}'"))
                    }
                };
                if metadata.len() > MAX_EXTRA_BYTES as u64 {
                    continue;
                }
                let source = match repo.read_worktree(&path) {
                    Ok(source) => source,
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                    {
                        continue
                    }
                    Err(error) => return Err(error),
                };
                if source.len() <= MAX_EXTRA_BYTES && contains_identifier(&search, &source) {
                    extra_files.push((path, source));
                }
            }
        }
        Ok(())
    })?;
    extra_files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((files, extra_files))
}

fn changed_name_search(files: &ChangedSources) -> Result<Option<aho_corasick::AhoCorasick>> {
    let mut terms = HashSet::new();
    for (path, base, head) in files {
        for source in [base, head].into_iter().flatten() {
            if let Some(parsed) = syntax::parse_file(path, source) {
                for function in parsed.fns {
                    for term in function
                        .name
                        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    {
                        if !term.is_empty() {
                            terms.insert(term.to_string());
                        }
                    }
                }
            }
        }
    }
    if terms.is_empty() {
        return Ok(None);
    }
    let mut terms: Vec<String> = terms.into_iter().collect();
    terms.sort();
    Ok(Some(
        aho_corasick::AhoCorasickBuilder::new()
            .match_kind(aho_corasick::MatchKind::Standard)
            .build(terms)
            .context("building changed-function identifier search")?,
    ))
}

fn contains_identifier(search: &aho_corasick::AhoCorasick, source: &str) -> bool {
    let bytes = source.as_bytes();
    let is_identifier = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    search.find_overlapping_iter(source).any(|hit| {
        (hit.start() == 0 || !is_identifier(bytes[hit.start() - 1]))
            && (hit.end() == bytes.len() || !is_identifier(bytes[hit.end()]))
    })
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

#[cfg(test)]
mod input_tests {
    use super::*;

    #[test]
    fn identifier_prefilter_respects_boundaries_and_overlapping_terms() {
        let search = aho_corasick::AhoCorasickBuilder::new()
            .match_kind(aho_corasick::MatchKind::Standard)
            .build(["run", "run_all"])
            .unwrap();
        assert!(contains_identifier(&search, "x.run()"));
        assert!(contains_identifier(&search, "run"));
        assert!(contains_identifier(&search, "run_all()"));
        assert!(!contains_identifier(
            &search,
            "rerun() run_all_more() _run run2"
        ));
        let run = aho_corasick::AhoCorasick::new(["run"]).unwrap();
        assert!(!contains_identifier(&run, "rerun() run_all()"));
        assert!(contains_identifier(&run, "rerun() x.run()"));
    }

    #[test]
    fn changed_names_produce_maximal_ascii_identifier_terms() {
        let files = vec![(
            "src/worker.cpp".to_string(),
            None,
            Some(
                "class Worker { public: ~Worker() {} int operator()() { return 1; } };".to_string(),
            ),
        )];
        let search = changed_name_search(&files)
            .unwrap()
            .expect("function names");
        assert!(contains_identifier(&search, "Worker object;"));
        assert!(contains_identifier(&search, "operator()"));
        assert!(!contains_identifier(&search, "WorkerPool operators"));
    }
}
