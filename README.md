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

Unchanged functions on a path to a change are kept (white) so you keep the
mental model; `f` toggles focus mode to hide them.

## Usage

```bash
# review the working tree against HEAD~1
calldive

# review a branch against main
calldive main feature/my-branch

# review only one function's subtree
calldive HEAD -e process_refund

# machine-readable tree (no TUI)
calldive HEAD --dump

# show unchanged entry points too
calldive --all

# include functions inside test modules / test files (hidden by default)
calldive --tests
```

Keys: `j/k` move · `enter` expand/collapse · `x` expand subtree ·
`f` focus changes · `tab` switch pane · `h` collapse · `q` quit.

## How it works

- Changed files are parsed at both revisions; symbols are matched by
  `file::name` identity with whitespace-normalized body hashing. Renamed or
  copied files are compared against their original path, so a pure file
  rename reports no function churn.
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
- A removed + an added function with identical bodies is reported as a move,
  so renames don't show phantom churn.
- Call sites resolve by simple name: same-file definition first, then a
  unique repo-wide match. Unresolved calls are shown as `(ext)`.
- Unchanged files that mention a changed function name are parsed too (head
  revision only, read through one `git cat-file --batch` process) to find
  callers outside the diff.
- Entry points are the top of every changed chain; review order is
  changed-first. File labels disambiguate colliding basenames
  (`crates/core/lib.rs` vs `crates/dap/lib.rs`).

## Limitations (v0.2)

- Name-based call resolution: no type inference, so dynamic dispatch,
  interface fan-out, and same-name methods resolve heuristically or not at all.
- Symbols use `file::name` identities: overloads and same-name methods within
  one file collapse to a single symbol. C/C++ macros are not expanded, and
  implicit or type-dependent calls are not resolved.
- C# property/accessor bodies and operator declarations are not extracted;
  calls outside a named function (including top-level statements) are not
  modeled.
- Only *let-bound* closures are named symbols; closures passed inline as
  arguments (`.map(|x| ...)`) are attributed to the enclosing function.
- Removed functions keep their base-revision callees but no base callers
  beyond the diff scope.

## Prior art

[difftastic](https://github.com/Wilfred/difftastic) (structural line diffs),
[SemanticDiff](https://semanticdiff.com) (semantic diff, VS Code/GitHub),
[hopdiff](https://github.com/tautology-labs/hopdiff) (call-graph diff, static
output, TS only),
[calldiff](https://github.com/tanishqkancharla/calldiff) (call-stack diff,
static output). calldive's niche: an *interactive* review session — DFS from
entry points with a diff at every hop, in the terminal.
