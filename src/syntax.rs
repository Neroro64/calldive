//! Tree-sitter based extraction of function definitions and call sites.
//!
//! Language support is intentionally small but covers the big ones:
//! Rust, Python, TypeScript/TSX, JavaScript/JSX, Go.

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
    /// Hash of the function's full source text (whitespace-normalized).
    pub body_hash: u64,
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
    Some(extract(&tree, src.as_bytes()))
}

fn extract(tree: &Tree, src: &[u8]) -> ParsedFile {
    let mut out = ParsedFile::default();
    let root = tree.root_node();

    // fn-def node kinds and how to read their name
    fn is_fn_def(kind: &str) -> bool {
        matches!(
            kind,
            "function_item"              // rust
                | "function_definition"  // python
                | "function_declaration" // ts/js/go
                | "generator_function_declaration" // js
                | "method_definition"    // ts/js
                | "method_declaration" // go
        )
    }

    // Recursive walk tracking the innermost enclosing function.
    fn walk(node: tree_sitter::Node, src: &[u8], out: &mut ParsedFile, current_fn: Option<usize>) {
        let kind = node.kind();

        if is_fn_def(kind) {
            let name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(src).ok())
                .map(|s| s.to_string());
            if let Some(name) = name {
                // body hash over everything EXCEPT the name, so a pure
                // rename/move keeps its identity
                let mut hash_src = String::new();
                let mut hc = node.walk();
                if hc.goto_first_child() {
                    loop {
                        let ch = hc.node();
                        let is_name =
                            node.child_by_field_name("name").map(|n| n.id()) == Some(ch.id());
                        if !is_name {
                            hash_src.push_str(ch.utf8_text(src).unwrap_or_default());
                            hash_src.push('\u{1}');
                        }
                        if !hc.goto_next_sibling() {
                            break;
                        }
                    }
                }
                let idx = out.fns.len();
                out.fns.push(FnDef {
                    name,
                    start_row: node.start_position().row,
                    end_row: node.end_position().row + 1,
                    body_hash: hash_norm(&hash_src),
                });
                // recurse into the body with this fn as the enclosing one
                let mut cursor = node.walk();
                if cursor.goto_first_child() {
                    loop {
                        walk(cursor.node(), src, out, Some(idx));
                        if !cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
                return;
            }
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

        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                walk(cursor.node(), src, out, current_fn);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    walk(root, src, &mut out, None);
    out
}

fn is_call(kind: &str) -> bool {
    matches!(
        kind,
        "call_expression"     // rust/js/ts/go
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
}
