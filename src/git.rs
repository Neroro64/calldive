use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Thin wrapper over git plumbing, executed in the repo working dir.
pub struct GitRepo {
    pub root: PathBuf,
}

/// A file that differs between base and head.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    /// A = added, M = modified, D = deleted, R = renamed
    pub status: char,
}

impl GitRepo {
    pub fn open(path: &Path) -> Result<Self> {
        let out = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("failed to run git (is git installed?)")?;
        if !out.status.success() {
            bail!(
                "not a git repository: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let root = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        Ok(Self { root })
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .with_context(|| format!("git {:?} failed to spawn", args))?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// True if the ref resolves. `HEAD~1` fails on single-commit repos.
    pub fn ref_exists(&self, rev: &str) -> bool {
        self.run(&["rev-parse", "--verify", "--quiet", rev]).is_ok()
    }

    /// Files that differ between base and head. When `head` is the worktree
    /// sentinel we diff base against the working tree (includes staged+unstaged).
    pub fn changed_files(&self, base: &str, head: &str) -> Result<Vec<FileChange>> {
        let args: Vec<&str> = if is_worktree(head) {
            vec!["diff", "--name-status", "-M", "--find-renames", base]
        } else {
            vec!["diff", "--name-status", "-M", "--find-renames", base, head]
        };
        let text = self.run(&args)?;
        let mut files = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut parts = line.splitn(2, '\t');
            let st = parts.next().unwrap_or_default();
            let path = parts.next().unwrap_or_default().to_string();
            if path.is_empty() {
                continue;
            }
            // R100\told\tnew => take the new path (last tab field)
            let (status, path) = if st.starts_with('R') || st.starts_with('C') {
                let mut it = line.splitn(3, '\t');
                let s = it.next().unwrap_or_default();
                let _old = it.next().unwrap_or_default();
                let new = it.next().unwrap_or_default().to_string();
                (s.chars().next().unwrap_or('M'), new)
            } else {
                (st.chars().next().unwrap_or('M'), path)
            };
            files.push(FileChange { path, status });
        }
        Ok(files)
    }

    /// File content at a rev. `None` rev = working tree.
    pub fn read_file(&self, rev: Option<&str>, path: &str) -> Result<String> {
        match rev {
            None => {
                let p = self.root.join(path);
                Ok(std::fs::read_to_string(&p)
                    .with_context(|| format!("reading worktree file {}", p.display()))?)
            }
            Some(r) => self.run(&["show", &format!("{}:{}", r, path)]),
        }
    }

    /// All tracked source files at a rev (None = working tree index).
    pub fn tracked_files(&self, rev: Option<&str>) -> Result<Vec<String>> {
        let args: Vec<String> = match rev {
            None => vec!["ls-files".into(), "--cached".into()],
            Some(r) => vec![
                "ls-tree".into(),
                "-r".into(),
                "--name-only".into(),
                r.into(),
            ],
        };
        let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let text = self.run(&argrefs)?;
        Ok(text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(String::from)
            .collect())
    }
}

/// Sentinel head meaning "the working tree as it is on disk".
pub const WORKTREE: &str = "worktree";

pub fn is_worktree(s: &str) -> bool {
    s.eq_ignore_ascii_case(WORKTREE)
}

/// Resolve a user-facing ref to something `git show` accepts.
/// Worktree returns None (read from disk).
pub fn resolve_rev(repo: &GitRepo, rev: &str) -> Result<Option<String>> {
    if is_worktree(rev) {
        return Ok(None);
    }
    if !repo.ref_exists(rev) {
        bail!("ref '{}' does not resolve in this repository", rev);
    }
    Ok(Some(rev.to_string()))
}
