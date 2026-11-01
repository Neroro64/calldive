//! Tree-sitter based extraction of function definitions and call sites.
//!
//! Language support is intentionally small but covers the big ones:
//! Rust, Python, TypeScript/TSX, JavaScript/JSX, Go, C#, C, C++.
//!
//! Also captures let-bound closures/lambdas (Rust `let f = |x| ...`,
//! JS/TS `const f = (x) => ...`, Python `f = lambda x: ...`,
//! Go `f := func() ...`, C# `Func<int> f = () => ...`,
//! C++ `auto f = [] { ... }`) as named symbols so calls to the binding resolve.

use tree_sitter::{Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Go,
    CSharp,
    C,
    Cpp,
}

pub fn lang_for_path(path: &str) -> Option<Lang> {
    // Uppercase .C conventionally denotes C++, unlike lowercase .c.
    if path.ends_with(".C") {
        return Some(Lang::Cpp);
    }
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
    } else if p.ends_with(".cs") {
        Some(Lang::CSharp)
    } else if p.ends_with(".c") {
        Some(Lang::C)
    } else if matches!(
        p.rsplit('.').next(),
        Some(
            "h" | "cc"
                | "cpp"
                | "cxx"
                | "c++"
                | "hh"
                | "hpp"
                | "hxx"
                | "h++"
                | "ipp"
                | "inl"
                | "tpp"
        )
    ) {
        // .h is ambiguous; the C++ grammar also handles ordinary C headers.
        Some(Lang::Cpp)
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
fn file_is_test_by_convention(path: &str, lang: Lang) -> bool {
    let p = path.to_ascii_lowercase();
    let base = p.rsplit('/').next().unwrap_or(&p);
    let in_tests = p.starts_with("tests/") || p.contains("/tests/");
    let language_test = match lang {
        Lang::Rust => in_tests,
        Lang::CSharp => {
            // Match the PascalCase suffix, not words such as Contest/Latest.
            let original_base = path.rsplit('/').next().unwrap_or(path);
            let stem = original_base
                .rsplit_once('.')
                .map_or(original_base, |(stem, _)| stem);
            in_tests
                || matches!(stem, "test" | "tests")
                || stem.ends_with("Test")
                || stem.ends_with("Tests")
        }
        Lang::C | Lang::Cpp => {
            let stem = base.rsplit_once('.').map_or(base, |(stem, _)| stem);
            in_tests
                || stem.starts_with("test_")
                || stem.ends_with("_test")
                || stem.ends_with("_tests")
        }
        _ => false,
    };
    p.ends_with("_test.go")
        || (base.starts_with("test_") && base.ends_with(".py"))
        || base.ends_with("_test.py")
        || base.contains(".test.")
        || p.contains("__tests__/")
        || language_test
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
        Lang::CSharp => tree_sitter_c_sharp::LANGUAGE,
        Lang::C => tree_sitter_c::LANGUAGE,
        Lang::Cpp => tree_sitter_cpp::LANGUAGE,
    };
    parser
        .set_language(&language.into())
        .expect("grammar/language version mismatch");
    let tree = parser.parse(src, None)?;
    Some(extract(
        &tree,
        src.as_bytes(),
        lang,
        file_is_test_by_convention(path, lang),
    ))
}

fn extract(tree: &Tree, src: &[u8], lang: Lang, file_is_test: bool) -> ParsedFile {
    let mut out = ParsedFile::default();
    let root = tree.root_node();

    fn is_fn_def(kind: &str) -> bool {
        matches!(
            kind,
            "function_item"                    // rust
                | "function_definition"        // python/c/c++
                | "function_declaration"       // ts/js/go
                | "generator_function_declaration" // js
                | "method_definition"          // ts/js
                | "method_declaration"         // go/c#
                | "constructor_declaration"    // c#
                | "destructor_declaration"     // c#
                | "local_function_statement" // c#
        )
    }

    /// let-bound closure/lambda binding name, if this node declares one.
    fn binding_name(node: tree_sitter::Node, lang: Lang) -> Option<tree_sitter::Node> {
        let kind = node.kind();
        if lang == Lang::C {
            return None;
        }
        // C# variable initializers are unnamed children, not a `value` field.
        if lang == Lang::CSharp {
            if kind != "variable_declarator" {
                return None;
            }
            let mut cursor = node.walk();
            node.named_children(&mut cursor).find(|n| {
                matches!(
                    unparenthesized(*n).kind(),
                    "lambda_expression" | "anonymous_method_expression"
                )
            })?;
            return node.child_by_field_name("name");
        }
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
        if lang == Lang::Cpp {
            value = unparenthesized(value);
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
            Lang::CSharp => unreachable!(),
            Lang::C => false,
            Lang::Cpp => kind == "init_declarator" && value_kind == "lambda_expression",
        };
        if !is_closure {
            return None;
        }
        if lang == Lang::Cpp {
            return declarator_name(node.child_by_field_name("declarator")?);
        }
        if matches!(lang, Lang::Python) {
            // assignment: `left` is the target (identifier or pattern list)
            let left = node.child_by_field_name("left")?;
            if left.kind() == "identifier" {
                return Some(left);
            }
            let mut c = left.walk();
            if c.goto_first_child() {
                loop {
                    if c.node().kind() == "identifier" {
                        return Some(c.node());
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
        Some(name_node)
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

        if is_fn_def(kind) && (lang != Lang::CSharp || node.child_by_field_name("body").is_some()) {
            let name_node = if matches!(lang, Lang::C | Lang::Cpp) {
                node.child_by_field_name("declarator")
                    .and_then(declarator_name)
            } else {
                node.child_by_field_name("name")
            };
            if let Some(name_node) = name_node {
                let Some(name) = name_node.utf8_text(src).ok() else {
                    return;
                };
                let name = if kind == "destructor_declaration" {
                    format!("~{name}")
                } else {
                    name.to_string()
                };
                let idx = out.fns.len();
                out.fns.push(FnDef {
                    name,
                    start_row: node.start_position().row,
                    end_row: node.end_position().row + 1,
                    body_hash: if matches!(lang, Lang::C | Lang::Cpp) {
                        hash_without_name(node, name_node, src)
                    } else {
                        hash_excluding(node, src, &["name"])
                    },
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
        if let Some(name_node) = binding_name(node, lang) {
            let idx = out.fns.len();
            out.fns.push(FnDef {
                name: name_node.utf8_text(src).unwrap_or_default().to_string(),
                start_row: node.start_position().row,
                end_row: node.end_position().row + 1,
                body_hash: if lang == Lang::Cpp {
                    hash_without_name(node, name_node, src)
                } else {
                    hash_excluding(node, src, &["pattern", "targets", "left", "name"])
                },
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
            | "invocation_expression" // c#
            | "object_creation_expression" // c# constructors
            | "new_expression" // c++ constructors
    )
}

/// Extract the simple callee name from a call node's function expression.
fn callee_name(call: tree_sitter::Node, src: &[u8]) -> Option<String> {
    let expr = call
        .child_by_field_name("function")
        .or_else(|| call.child_by_field_name("type"))?;
    simple_name(expr, src)
}

/// Last identifier segment of any callable expression:
/// `foo`, `a.b.c()`, `Module::fn()`, `obj.method()`, `f::<T>()`.
fn simple_name(expr: tree_sitter::Node, src: &[u8]) -> Option<String> {
    match expr.kind() {
        "identifier"
        | "property_identifier"
        | "field_identifier"
        | "shorthand_property_identifier"
        | "type_identifier"
        | "destructor_name"
        | "operator_name" => expr.utf8_text(src).ok().map(|s| s.to_string()),
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
        "generic_name" => expr.named_child(0).and_then(|n| simple_name(n, src)),
        "template_function" | "template_method" | "template_type" | "dependent_name" => expr
            .child_by_field_name("name")
            .and_then(|n| simple_name(n, src)),
        "pointer_expression" => expr
            .child_by_field_name("argument")
            .and_then(|n| simple_name(n, src)),
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
        | "qualified_identifier"
        | "member_expression"
        | "member_access_expression"
        | "member_binding_expression"
        | "conditional_access_expression"
        | "qualified_name"
        | "alias_qualified_name" => {
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

fn unparenthesized(mut node: tree_sitter::Node) -> tree_sitter::Node {
    while node.kind() == "parenthesized_expression" {
        match node.named_child(0) {
            Some(inner) => node = inner,
            None => break,
        }
    }
    node
}

/// Follow only the declarator spine, never identifiers in parameters or types.
fn declarator_name(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" | "destructor_name"
        | "operator_name" => Some(node),
        "qualified_identifier" | "template_function" | "template_method" | "dependent_name" => {
            node.child_by_field_name("name").and_then(declarator_name)
        }
        "parenthesized_declarator" | "reference_declarator" => {
            node.named_child(0).and_then(declarator_name)
        }
        _ => node
            .child_by_field_name("declarator")
            .and_then(declarator_name),
    }
}

/// C/C++ names sit inside nested declarators. Retain the entire signature
/// while omitting only the name, so renames match but signature edits do not.
fn hash_without_name(node: tree_sitter::Node, name: tree_sitter::Node, src: &[u8]) -> u64 {
    // The tilde distinguishes a destructor from a constructor, not its name.
    let name_start = name.start_byte() + usize::from(name.kind() == "destructor_name");
    let before = std::str::from_utf8(&src[node.start_byte()..name_start]).unwrap_or_default();
    let after = std::str::from_utf8(&src[name.end_byte()..node.end_byte()]).unwrap_or_default();
    let mut h = 0xcbf29ce484222325u64;
    let mut started = false;
    let mut pending_ws = false;
    for ch in before.chars().chain(after.chars()) {
        if ch.is_whitespace() {
            pending_ws = started;
            continue;
        }
        if pending_ws {
            h = (h ^ u64::from(b' ')).wrapping_mul(0x100000001b3);
            pending_ws = false;
        }
        let mut bytes = [0; 4];
        for &b in ch.encode_utf8(&mut bytes).as_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
        }
        started = true;
    }
    h
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
    fn csharp_methods_constructors_and_nested_callables() {
        let src = r#"
interface IWorker { int Missing(); }
abstract class Base { public abstract int Absent(); }
class Worker {
    public Worker() { Setup(); }
    ~Worker() { Cleanup(); }
    int Helper(int x) => x + 1;
    int Run() {
        int Local(int x) => Helper(x);
        Func<int, int> twice = (x => Local(x));
        Func<int> fallback = delegate { return Helper(0); };
        receiver?.Send<int>(twice(2));
        Worker.Create<string>();
        var worker = new Worker();
        return fallback();
    }
}
"#;
        let pf = parse_file("Worker.cs", src).unwrap();
        let names: Vec<_> = pf.fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            ["Worker", "~Worker", "Helper", "Run", "Local", "twice", "fallback"]
        );
        let calls: Vec<_> = pf
            .calls
            .iter()
            .map(|c| (pf.fns[c.caller].name.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("Worker", "Setup"),
                ("~Worker", "Cleanup"),
                ("Local", "Helper"),
                ("twice", "Local"),
                ("fallback", "Helper"),
                ("Run", "Send"),
                ("Run", "twice"),
                ("Run", "Create"),
                ("Run", "Worker"),
                ("Run", "fallback"),
            ]
        );
        let run = &pf.fns[3];
        assert!(src
            .lines()
            .nth(run.start_row)
            .unwrap()
            .contains("int Run()"));
        assert_eq!(src.lines().nth(run.end_row - 1).unwrap().trim(), "}");
    }

    #[test]
    fn c_definitions_follow_nested_declarators_and_ignore_prototypes() {
        let src = r#"
int forward(int value);
int helper(int value) { return value + 1; }
char *pointer(void) { return external(); }
int (*factory(void))(int) { return helper; }
int run(int (*callback)(int)) {
    return helper(callback(2)) + (*callback)(3);
}
"#;
        let pf = parse_file("app.c", src).unwrap();
        let names: Vec<_> = pf.fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["helper", "pointer", "factory", "run"]);
        let calls: Vec<_> = pf
            .calls
            .iter()
            .map(|c| (pf.fns[c.caller].name.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("pointer", "external"),
                ("run", "helper"),
                ("run", "callback"),
                ("run", "callback"),
            ]
        );
    }

    #[test]
    fn cpp_headers_methods_templates_and_bound_lambdas() {
        let src = r#"
namespace api {
    template<typename T> T helper(T x) { return x + 1; }
}
struct Worker {
    Worker() { init(); }
    ~Worker() { cleanup(); }
    int run();
    int operator()(int x) { return api::helper<int>(x); }
};
int &reference() { return value; }
int Worker::run() {
    auto twice = ([](int x) { return api::helper<int>(x); });
    worker->send<int>(twice(2));
    auto other = new Worker();
    return (*callback)(3);
}
"#;
        let pf = parse_file("worker.h", src).unwrap();
        let names: Vec<_> = pf.fns.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "helper",
                "Worker",
                "~Worker",
                "operator()",
                "reference",
                "run",
                "twice"
            ]
        );
        let calls: Vec<_> = pf
            .calls
            .iter()
            .map(|c| (pf.fns[c.caller].name.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("Worker", "init"),
                ("~Worker", "cleanup"),
                ("operator()", "helper"),
                ("twice", "helper"),
                ("run", "send"),
                ("run", "twice"),
                ("run", "Worker"),
                ("run", "callback"),
            ]
        );
    }

    #[test]
    fn native_renames_preserve_hashes_but_signature_changes_do_not() {
        for (path, source) in [
            ("app.c", "int *old(int x) { return helper(x); }"),
            ("app.cpp", "int &Worker::old(int x) { return helper(x); }"),
            ("app.C", "auto old = [](int x) { return helper(x); };"),
            ("app.cs", "class App { int old(int x) => helper(x); }"),
        ] {
            let base = parse_file(path, source).unwrap();
            let renamed = parse_file(path, &source.replace("old", "renamed")).unwrap();
            assert_eq!(base.fns[0].name, "old", "{path}");
            assert_eq!(renamed.fns[0].name, "renamed", "{path}");
            assert_eq!(base.fns[0].body_hash, renamed.fns[0].body_hash, "{path}");
            let changed = parse_file(path, &source.replace("int x", "long x")).unwrap();
            assert_ne!(base.fns[0].body_hash, changed.fns[0].body_hash, "{path}");
        }
        let methods =
            parse_file("worker.cpp", "struct Worker { Worker() {} ~Worker() {} };").unwrap();
        assert_ne!(
            methods.fns[0].body_hash, methods.fns[1].body_hash,
            "a constructor becoming a destructor is not a pure rename"
        );
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
        for (path, production, source) in [
            (
                "WorkerTests.cs",
                "Worker.cs",
                "class Worker { void Run() {} }",
            ),
            (
                "tests/worker.cs",
                "src/worker.cs",
                "class Worker { void Run() {} }",
            ),
            ("test_worker.c", "worker.c", "void run(void) {}"),
            ("worker_test.cpp", "worker.cpp", "void run() {}"),
            ("tests/worker.hpp", "src/worker.hpp", "inline void run() {}"),
        ] {
            assert!(parse_file(path, source).unwrap().fns[0].is_test, "{path}");
            assert!(
                !parse_file(production, source).unwrap().fns[0].is_test,
                "{production}"
            );
        }
        let contest = parse_file("Contest.cs", "class Contest { void Run() {} }").unwrap();
        assert!(
            !contest.fns[0].is_test,
            "production names ending in lowercase 'test' stay visible"
        );
    }
}
