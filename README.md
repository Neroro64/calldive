# calldive

Interactive TUI code review for the agentic era. Built with Rust + ratatui.

AI agents produce large diffs fast. A flat, file-by-file line diff buries the
one thing a senior reviewer actually needs: **what behavior changed and how it
flows**. calldive reviews a change the way it executes:

1. Parse the changed files of a git range with tree-sitter (Rust, Python,
   TypeScript/TSX, JavaScript/JSX, Go, C#, C, C++). In worktree mode, untracked
   (non-ignored) files are reviewed as added files.
2. Build the call graph at both revisions and diff the graphs.
3. Start at the **entry points** and DFS into callees — every hop shows the
   per-function line diff, its callers, and its callees.

## Status glyphs

- `~` body changed
- `+` added
- `-` removed
- `>` renamed/moved (body hash identical)

Unchanged functions that transitively call a change remain relevant, preserving
the route into changed behavior. `f` hides branches that are not relevant.

## Usage

```bash
# review the working tree (including non-ignored untracked files) against HEAD~1
calldive
# review a branch against main
calldive main feature/my-branch
# scope to a simple name, container-qualified name, path suffix, or exact node key
calldive HEAD --entry process_refund
calldive HEAD --entry 'Worker::run'
calldive HEAD --entry 'src/x.rs::run'
calldive HEAD --entry 'x.rs::run'
# non-interactive review tree or complete machine-readable analysis
calldive HEAD --dump
calldive HEAD --json
# include clean root components / test functions
calldive --all
calldive --tests
# initially hide branches that do not reach a change (TUI only)
calldive --focus
```

An exact `--entry` node key wins. Otherwise a simple or qualified name, or a
path-qualified suffix, must match exactly one function; ambiguous matches list
every candidate key in sorted order. `--json` and `--dump` are mutually exclusive.

Dump rows are `{indent}{glyph} {name} :{label}  {start}-{end}{kids}`: two spaces
per depth, 1-based inclusive revision lines, and ` …` for hidden children.
Removed functions use base lines; other functions use head lines. Root nodes
start expanded; `--focus` does not alter dump output.

JSON is one document with `base`, `head`, `summary` (`added`, `removed`,
`changed`, `moved`), selected `roots`, all `entry_points`, and `nodes` sorted by
key. Each node contains `key`, `name`, `file`, `label`, `status` (`added`,
`removed`, `changed`, `moved`, `unchanged`), nullable `head` and `base` spans
(`file`, 1-based inclusive `start`/`end`), nullable `moved_from`, `relevant`,
`callers`, `external_callers`, `callees`, and `lost_callees`.

Keys: `j/k` move · `enter`/`l` toggle expansion · `h` collapse the row or its
nearest expanded ancestor · `x` expand subtree (depth limit 5) · `e` collapse
to roots · `f` focus relevant branches · `g/G` first/last row (top/bottom in
details) · `tab` switch pane · `Shift+H`/`Shift+L` scroll the focused panel
left/right by 8 columns (each panel keeps its own offset) · `q` quit.
In the diff panel, `Ctrl+D` / `Ctrl+U` scroll down/up by half a visible page,
stopping at the bottom/top.
The tree starts scrolled past the first selected symbol's namespace prefix;
use `Shift+H` to reveal the full qualification.
In the detail pane, diff markers are
`+` added lines, `-` removed lines, and a space for context; base and head line
numbers refer to their own revisions.

## How it works

- Changed files are parsed at both revisions. Identities are `file::qual`,
  where `qual` includes containers (for example `A::new`, `Worker::Run`, or
  `ns::Worker::run`). If a file has multiple definitions with the same qual,
  their whitespace-normalized signature text is appended (`Worker::Log(int x)`);
  remaining duplicates receive appearance-order `#n` suffixes.
- Functions pair by qualified identity and signature before comparing body
  hashes. Renamed or copied files are compared against their original path, so
  a pure file rename reports no function churn. Remaining identical-body
  additions/removals are deterministically paired as moves; the origin's name
  and base span are retained even when its file differs from the destination.
- Sources are decoded as UTF-8 with invalid bytes replaced and a leading BOM
  stripped, identically for working-tree files and git objects.
- TypeScript uses `.ts`/`.mts`/`.cts`, TSX uses the TSX grammar for `.tsx`,
  and JavaScript uses `.js`/`.jsx`/`.mjs`/`.cjs`.
- C# (`.cs`) includes methods, constructors/destructors, local functions,
  and bound lambdas/anonymous delegates. C/C++ includes function definitions,
  methods and templates; declarations/prototypes are not separate symbols.
- C uses `.c`; C++ uses `.C`, `.cc`, `.cpp`, `.cxx`, `.c++`, `.h`, `.hh`,
  `.hpp`, `.hxx`, `.h++`, `.ipp`, `.inl`, and `.tpp`. Ambiguous `.h` headers
  use the C++ grammar, which also handles ordinary C header definitions.
- Let-bound closures/lambdas are captured as named symbols (Rust
  `let f = |x| ...`, JS/TS `const f = (x) => ...`, Python `f = lambda x: ...`,
  Go `f := func() ...`, C# `Func<int> f = () => ...`, C++ `auto f = [] { ... }`),
  so calls through the binding resolve.
- Test functions are hidden by default; `--tests` includes them. Conventions:
  `tests/` dirs (every language except Go; also `test/` for Python and JS/TS), Rust
  `#[test]`-style attributes and `#[cfg(test)]` modules, `*_test.go`,
  `test_*.py` / `*_test.py` / `conftest.py`, `*.test.*` / `*.spec.*` /
  `__tests__`, C# `*Test.cs` / `*Tests.cs` plus `[Fact]`/`[Theory]`/`[Test]`/
  `[TestMethod]`/`[TestCase]` methods and `[TestFixture]`/`[TestClass]`
  classes, and C/C++ `test_*` / `*_test` / `*_tests` files.
- Calls resolve by simple name to the same-file overload set first. Otherwise
  there must be a unique definition in the same language family across changed
  and unchanged files (JS/TS/TSX share a family, as do C/C++). Ambiguous or
  unresolved calls remain `(ext)`. Removed functions retain base calls, with a
  removed-definition fallback when the head graph has no target.
- Unchanged files that mention changed function names are parsed at head to
  find callers and disambiguate definitions. Changed blobs and the batched
  unchanged-file scan share one `git cat-file --batch` process; oversized
  unchanged sources are skipped.
- Entry points are root components of the in-scope call graph, including a
  deterministic representative of recursive cycles. Self-recursion does not
  hide a root. Callers in unchanged files appear as `(outside diff)` in details
  but do **not** disqualify an entry point. Review order is deterministic and
  changed-first. A function is relevant if it changes or transitively calls a
  change; default roots are relevant, while `--all` includes clean roots.
- File labels are basenames unless distinct paths collide, then the shortest
  unique whole-segment suffix (`a/lib.rs`, `b/lib.rs`). Root-level files keep
  their plain filename, never a leading slash.

## Limitations

- Name-based resolution has no type inference: receiver types, dynamic
  dispatch, interface fan-out, and implicit/type-dependent calls are not
  resolved precisely. Overloads are disambiguated by signature text only,
  not argument types.
- C/C++ macros are not expanded.
- C# properties/accessors and operator declarations are not extracted;
  calls outside named functions (including top-level statements) are not modeled.
- Only bound closures/lambdas become named symbols. Inline closures passed
  as arguments (such as `.map(|x| ...)`) are attributed to their enclosing fn.
- Removed functions retain base-revision callees, but callers outside the diff
  are scanned only at head, not at base.

## Prior art

[difftastic](https://github.com/Wilfred/difftastic) (structural line diffs),
[SemanticDiff](https://semanticdiff.com) (semantic diff, VS Code/GitHub),
[hopdiff](https://github.com/tautology-labs/hopdiff) (call-graph diff, static
output, TS only),
[calldiff](https://github.com/tanishqkancharla/calldiff) (call-stack diff,
static output). calldive's niche: an *interactive* review session — DFS from
entry points with a diff at every hop, in the terminal.
