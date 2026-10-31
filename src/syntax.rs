//! Tree-sitter based extraction of function definitions and call sites.
//!
//! Language support is intentionally small but covers the big ones:
//! Rust, Python, TypeScript/TSX, JavaScript/JSX, Go.
//!
//! Also captures let-bound closures/lambdas (Rust `let f = |x| ...`,
//! JS/TS `const f = (x) => ...`, Python `f = lambda x: ...`,
//! Go `f := func() ...`) as named symbols so calls to the binding resolve.

use tree_sitter::{Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Go,
}

pub fn lang_for_path(path: &str) -> Option<Lang> {
    let p = path.to_ascii_lowercase();
    if p.ends_with(".rs") {
        Some(Lang::Rust)
    } else if p.ends_with(".py") {
        Some(Lang::Python)
    } else if p.ends_with(".ts") || p.ends_with(".tsx") {
        Some(Lang::TypeScript)
    } else if p.ends_with(".js") || p.ends_with(".jsx") || p.ends_with(".mjs") {
        Some(Lang::JavaScript)
    } else if p.ends_with(".go") {
        Some(Lang::Go)
    } else {
        None
    }
}

/// A function definition found in one file revision.
#[derive(Debug, Clone)]
pub struct FnDef {
    pub name: String,
    /// 0-based start row (inclusive)
    pub start_row: usize,
    /// 0-based end row (exclusive)
    pub end_row: usize,
    /// Hash of the function's full source text (whitespace-normalized),
    /// excluding the name so pure renames keep their identity.
    pub body_hash: u64,
    /// lives inside a test module / test file (convention-based)
    pub is_test: bool,
}

/// A call made inside a function (attributed to the innermost enclosing fn).
#[derive(Debug, Clone)]
pub struct CallSite {
    /// index into the same file's `fns` vec
    pub caller: usize,
    /// simple name of the callee (last path segment)
    pub name: String,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedFile {
    pub fns: Vec<FnDef>,
    pub calls: Vec<CallSite>,
}

/// File-level test convention (independent of AST).
fn file_is_test_by_convention(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let base = p.rsplit('/').next().unwrap_or(&p).to_string();
    let rust_integration_test =
        p.ends_with(".rs") && (p.starts_with("tests/") || p.contains("/tests/"));
    p.ends_with("_test.go")
        || (base.starts_with("test_") && base.ends_with(".py"))
        || base.ends_with("_test.py")
        || base.contains(".test.")
        || p.contains("__tests__/")
        || rust_integration_test
}

pub fn parse_file(path: &str, src: &str) -> Option<ParsedFile> {
    let lang = lang_for_path(path)?;
    let mut parser = Parser::new();
    let language = match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE,
        Lang::Python => tree_sitter_python::LANGUAGE,
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE,
        Lang::Go => tree_sitter_go::LANGUAGE,
    };
    parser
        .set_language(&language.into())
        .expect("grammar/language version mismatch");
    let tree = parser.parse(src, None)?;
    Some(extract(
        &tree,
        src.as_bytes(),
        lang,
        file_is_test_by_convention(path),
    ))
}

fn extract(tree: &Tree, src: &[u8], lang: Lang, file_is_test: bool) -> ParsedFile {
    let mut out = ParsedFile::default();
    let root = tree.root_node();

    fn is_fn_def(kind: &str) -> bool {
        matches!(
            kind,
            "function_item"                    // rust
                | "function_definition"        // python
                | "function_declaration"       // ts/js/go
                | "generator_function_declaration" // js
                | "method_definition"          // ts/js
                | "method_declaration" // go
        )
    }

    /// let-bound closure/lambda binding name, if this node declares one.
    fn binding_name(node: tree_sitter::Node, lang: Lang, src: &[u8]) -> Option<String> {
        let kind = node.kind();
        let mut value = node
            .child_by_field_name("value")
            .or_else(|| node.child_by_field_name("right"))?;
        // go: right is an expression_list wrapping the actual value
        if value.kind() == "expression_list" {
            let mut c = value.walk();
            if !c.goto_first_child() {
                return None;
            }
            value = c.node();
        }
        let value_kind = value.kind();
        let is_closure = match lang {
            Lang::Rust => kind == "let_declaration" && value_kind == "closure_expression",
            Lang::Python => kind == "assignment" && value_kind == "lambda",
            Lang::Go => kind == "short_var_declaration" && value_kind == "func_literal",
            Lang::TypeScript | Lang::JavaScript => {
                kind == "variable_declarator"
                    && matches!(
                        value_kind,
                        "arrow_function" | "function_expression" | "function"
                    )
            }
        };
        if !is_closure {
            return None;
        }
        if matches!(lang, Lang::Python) {
            // assignment: `left` is the target (identifier or pattern list)
            let left = node.child_by_field_name("left")?;
            if left.kind() == "identifier" {
                return left.utf8_text(src).ok().map(String::from);
            }
            let mut c = left.walk();
            if c.goto_first_child() {
                loop {
                    if c.node().kind() == "identifier" {
                        return c.node().utf8_text(src).ok().map(String::from);
                    }
                    if !c.goto_next_sibling() {
                        break;
                    }
                }
            }
            return None;
        }
        let name_field = match lang {
            Lang::Rust => "pattern",
            Lang::TypeScript | Lang::JavaScript => "name",
            Lang::Go => "left",
            _ => unreachable!(),
        };
        let name_node = node.child_by_field_name(name_field)?;
        // go: left is an expression_list wrapping the identifier
        let name_node = if name_node.kind() == "expression_list" {
            let mut c = name_node.walk();
            if !c.goto_first_child() {
                return None;
            }
            c.node()
        } else {
            name_node
        };
        name_node.utf8_text(src).ok().map(|s| s.to_string())
    }

    // Recursive walk tracking the innermost enclosing function and whether
    // we are inside a test module (Rust `mod tests` convention).
    fn walk(
        node: tree_sitter::Node,
        src: &[u8],
        out: &mut ParsedFile,
        lang: Lang,
        file_is_test: bool,
        in_test_mod: bool,
        current_fn: Option<usize>,
    ) {
        let kind = node.kind();

        // Rust: entering `mod tests { ... }` (grammar kind: mod_item)
        let child_in_test_mod = if kind == "mod_item" && node.child_by_field_name("name").is_some()
        {
            let name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(src).ok())
                .unwrap_or_default();
            in_test_mod || name == "tests"
        } else {
            in_test_mod
        };

        let is_test_here = file_is_test || child_in_test_mod;

        if is_fn_def(kind) {
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(src).ok())
                .map(|s| s.to_string())
            {
                let idx = out.fns.len();
                out.fns.push(FnDef {
                    name,
                    start_row: node.start_position().row,
                    end_row: node.end_position().row + 1,
                    body_hash: hash_excluding(node, src, &["name"]),
                    is_test: is_test_here,
                });
                recurse_children(
                    node,
                    src,
                    out,
                    lang,
                    file_is_test,
                    child_in_test_mod,
                    Some(idx),
                );
                return;
            }
        }

        // let-bound closure/lambda as a named symbol
        if let Some(name) = binding_name(node, lang, src) {
            let idx = out.fns.len();
            out.fns.push(FnDef {
                name,
                start_row: node.start_position().row,
                end_row: node.end_position().row + 1,
                body_hash: hash_excluding(node, src, &["pattern", "targets", "left", "name"]),
                is_test: is_test_here,
            });
            recurse_children(
                node,
                src,
                out,
                lang,
                file_is_test,
                child_in_test_mod,
                Some(idx),
            );
            return;
        }

        if is_call(kind) {
            if let Some(caller) = current_fn {
                if let Some(callee) = callee_name(node, src) {
                    out.calls.push(CallSite {
                        caller,
                        name: callee,
                    });
                }
            }
        }

        recurse_children(
            node,
            src,
            out,
            lang,
            file_is_test,
            child_in_test_mod,
            current_fn,
        );
    }

    fn recurse_children(
        node: tree_sitter::Node,
        src: &[u8],
        out: &mut ParsedFile,
        lang: Lang,
        file_is_test: bool,
        in_test_mod: bool,
        current_fn: Option<usize>,
    ) {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                walk(
                    cursor.node(),
                    src,
                    out,
                    lang,
                    file_is_test,
                    in_test_mod,
                    current_fn,
                );
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    walk(root, src, &mut out, lang, file_is_test, false, None);
    out
}

fn is_call(kind: &str) -> bool {
    matches!(
        kind,
        "call_expression"        // rust/js/ts/go
            | "generic_function" // rust turbofish
            | "call" // python
    )
}

/// Extract the simple callee name from a call node's function expression.
fn callee_name(call: tree_sitter::Node, src: &[u8]) -> Option<String> {
    // every supported grammar names the callee field "function"
    let expr = call.child_by_field_name("function")?;
    simple_name(expr, src)
}

/// Last identifier segment of any callable expression:
/// `foo`, `a.b.c()`, `Module::fn()`, `obj.method()`, `f::<T>()`.
fn simple_name(expr: tree_sitter::Node, src: &[u8]) -> Option<String> {
    match expr.kind() {
        "identifier"
        | "property_identifier"
        | "field_identifier"
        | "shorthand_property_identifier" => expr.utf8_text(src).ok().map(|s| s.to_string()),
        "parenthesized_expression" => {
            let mut inner = None;
            let mut c = expr.walk();
            if c.goto_first_child() {
                loop {
                    if c.node().is_named() {
                        inner = Some(c.node());
                        break;
                    }
                    if !c.goto_next_sibling() {
                        break;
                    }
                }
            }
            inner.and_then(|n| simple_name(n, src))
        }
        "generic_function" => {
            // rust: function field holds the base expression
            expr.child_by_field_name("function")
                .and_then(|n| simple_name(n, src))
        }
        // member chains: a.b.c / Module::fn / obj.method
        "field_expression"
        | "attribute"
        | "selector_expression"
        | "scoped_identifier"
        | "member_expression" => {
            let mut last = None;
            let mut c = expr.walk();
            if c.goto_first_child() {
                loop {
                    if c.node().is_named() {
                        last = Some(c.node());
                    }
                    if !c.goto_next_sibling() {
                        break;
                    }
                }
            }
            last.and_then(|n| simple_name(n, src))
        }
        _ => None,
    }
}

/// Hash the node's children except the given identifying fields,
/// whitespace-normalized.
fn hash_excluding(node: tree_sitter::Node, src: &[u8], skip_fields: &[&str]) -> u64 {
    let mut hash_src = String::new();
    let mut hc = node.walk();
    if hc.goto_first_child() {
        loop {
            let ch = hc.node();
            let skipped = skip_fields
                .iter()
                .any(|f| node.child_by_field_name(f).map(|n| n.id()) == Some(ch.id()));
            if !skipped {
                hash_src.push_str(ch.utf8_text(src).unwrap_or_default());
                hash_src.push('\u{1}');
            }
            if !hc.goto_next_sibling() {
                break;
            }
        }
    }
    hash_norm(&hash_src)
}

fn hash_norm(text: &str) -> u64 {
    // normalize whitespace runs to single spaces so pure reformatting
    // inside a line does not count as a body change
    let mut norm = String::with_capacity(text.len());
    let mut prev_ws = false;
    for ch in text.chars() {
        let ws = ch.is_whitespace();
        if ws {
            if !prev_ws {
                norm.push(' ');
            }
        } else {
            norm.push(ch);
        }
        prev_ws = ws;
    }
    // FNV-1a 64
    let mut h: u64 = 0xcbf29ce484222325;
    for b in norm.trim().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_fns_and_calls() {
        let src = r#"
fn helper(x: u32) -> u32 { x + 1 }

fn main() {
    let y = helper(2);
    std::mem::drop(y);
}
"#;
        let pf = parse_file("a.rs", src).unwrap();
        assert_eq!(pf.fns.len(), 2);
        assert_eq!(pf.fns[0].name, "helper");
        assert_eq!(pf.fns[1].name, "main");
        let calls: Vec<_> = pf.calls.iter().map(|c| c.name.as_str()).collect();
        assert!(calls.contains(&"helper"), "{calls:?}");
        assert!(calls.contains(&"drop"), "{calls:?}");
        // helper call belongs to main (index 1)
        let main_calls: Vec<_> = pf
            .calls
            .iter()
            .filter(|c| c.caller == 1)
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(main_calls.len(), 2);
    }

    #[test]
    fn python_fns_and_calls() {
        let src = r#"
def helper(x):
    return x + 1

def main():
    y = helper(2)
    print(y)
"#;
        let pf = parse_file("a.py", src).unwrap();
        assert_eq!(pf.fns.len(), 2);
        let names: Vec<_> = pf.calls.iter().map(|c| c.name.clone()).collect();
        assert!(names.contains(&"helper".to_string()));
        assert!(names.contains(&"print".to_string()));
    }

    #[test]
    fn ts_methods_and_calls() {
        let src = r#"
export function handler(req: Request) {
  return svc.process(req);
}

class Svc {
  process(req: Request) { return req; }
}
"#;
        let pf = parse_file("a.ts", src).unwrap();
        assert_eq!(pf.fns.len(), 2);
        let names: Vec<_> = pf.calls.iter().map(|c| c.name.clone()).collect();
        assert!(names.contains(&"process".to_string()));
    }

    #[test]
    fn rust_mod_tests_marked() {
        let src = r#"
fn prod() { helper(); }
fn helper() {}

#[cfg(test)]
mod tests {
    fn test_thing() { helper(); }
}
"#;
        let pf = parse_file("a.rs", src).unwrap();
        let by: Vec<(&String, bool)> = pf.fns.iter().map(|f| (&f.name, f.is_test)).collect();
        assert_eq!(by.len(), 3);
        assert!(!by.iter().find(|(n, _)| *n == "prod").unwrap().1);
        assert!(by.iter().find(|(n, _)| *n == "test_thing").unwrap().1);
        // helper defined at top level is not test even though tests call it
        assert!(!by.iter().find(|(n, _)| *n == "helper").unwrap().1);
    }

    #[test]
    fn closures_captured_as_named_symbols() {
        let rust =
            "fn main() {\n    let double = |x: u32| helper(x);\n    double(2);\n}\nfn helper(x: u32) -> u32 { x * 2 }\n";
        let pf = parse_file("a.rs", rust).unwrap();
        assert!(pf.fns.iter().any(|f| f.name == "double"), "{:?}", pf.fns);
        // calls inside the closure attribute to the closure (index of double)
        let double_idx = pf.fns.iter().position(|f| f.name == "double").unwrap();
        assert!(pf
            .calls
            .iter()
            .any(|c| c.caller == double_idx && c.name == "helper"));

        let ts = "const retry = (fn) => { return fn(); };\nexport function run() { return retry(work); }\n";
        let pf = parse_file("a.ts", ts).unwrap();
        assert!(pf.fns.iter().any(|f| f.name == "retry"), "{:?}", pf.fns);

        let py = "def run():\n    f = lambda x: helper(x)\n    return f(1)\n";
        let pf = parse_file("a.py", py).unwrap();
        assert!(pf.fns.iter().any(|f| f.name == "f"), "{:?}", pf.fns);

        let go = "func run() {\n    f := func() int { return helper() }\n    _ = f()\n}\n";
        let pf = parse_file("a.go", go).unwrap();
        assert!(pf.fns.iter().any(|f| f.name == "f"), "{:?}", pf.fns);
    }

    #[test]
    fn file_convention_test_detection() {
        let pf = parse_file("tests/it.rs", "fn a() {}\n").unwrap(); // rust integration test dir
        assert!(pf.fns[0].is_test);
        let pf = parse_file("src/it.rs", "fn a() {}\n").unwrap(); // rust: no file convention
        assert!(!pf.fns[0].is_test);
        let pf = parse_file("x_test.go", "func a() {}\n").unwrap();
        assert!(pf.fns[0].is_test);
        let pf = parse_file("test_db.py", "def a(): pass\n").unwrap();
        assert!(pf.fns[0].is_test);
        let pf = parse_file("src/app.test.ts", "function a() {}\n").unwrap();
        assert!(pf.fns[0].is_test);
    }
}
