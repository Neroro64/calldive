//! Tree-sitter based extraction of function definitions and call sites.
//!
//! Language support is intentionally small but covers the big ones:
//! Rust, Python, TypeScript/TSX, JavaScript/JSX, Go, C#, C, C++.
//!
//! Also captures let-bound closures/lambdas (Rust `let f = |x| ...`,
//! JS/TS `const f = (x) => ...`, Python `f = lambda x: ...`,
//! Go `f := func() ...`, C# `Func<int> f = () => ...`,
//! C++ `auto f = [] { ... }`) as named symbols so calls to the binding resolve.
//!
//! Identities retain containers and overload signatures; extraction uses a
//! stack-safe pre-order cursor and reuses one parser per language per thread.

use std::cell::RefCell;
use std::collections::HashMap;
use tree_sitter::{Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
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
    } else if p.ends_with(".tsx") {
        Some(Lang::Tsx)
    } else if p.ends_with(".ts") || p.ends_with(".mts") || p.ends_with(".cts") {
        Some(Lang::TypeScript)
    } else if p.ends_with(".js")
        || p.ends_with(".jsx")
        || p.ends_with(".mjs")
        || p.ends_with(".cjs")
    {
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

impl Lang {
    pub fn family(self) -> Lang {
        match self {
            Lang::JavaScript | Lang::Tsx => Lang::TypeScript,
            Lang::C => Lang::Cpp,
            other => other,
        }
    }
}

thread_local! {
    static PARSERS: RefCell<HashMap<Lang, Parser>> = RefCell::new(HashMap::new());
}

/// A function definition found in one file revision.
#[derive(Debug, Clone)]
pub struct FnDef {
    pub name: String,
    /// Container path ending in the simple function name.
    pub qual: String,
    /// Whitespace-normalized parameter/declarator suffix for overload identity.
    pub sig: String,
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
    let in_test = in_tests || p.starts_with("test/") || p.contains("/test/");
    let language_test = match lang {
        Lang::Rust => in_tests,
        Lang::Python => in_test || base == "conftest.py",
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => in_test || base.contains(".spec."),
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
    let tree = PARSERS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let parser = cache.entry(lang).or_insert_with(|| {
            let mut parser = Parser::new();
            let language = match lang {
                Lang::Rust => tree_sitter_rust::LANGUAGE,
                Lang::Python => tree_sitter_python::LANGUAGE,
                Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX,
                Lang::JavaScript => tree_sitter_javascript::LANGUAGE,
                Lang::Go => tree_sitter_go::LANGUAGE,
                Lang::CSharp => tree_sitter_c_sharp::LANGUAGE,
                Lang::C => tree_sitter_c::LANGUAGE,
                Lang::Cpp => tree_sitter_cpp::LANGUAGE,
            };
            parser
                .set_language(&language.into())
                .expect("grammar version mismatch");
            parser
        });
        parser.parse(src, None)
    })?;
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
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
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
            Lang::TypeScript | Lang::Tsx | Lang::JavaScript => "name",
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

    // Cursor traversal preserves pre-order without consuming the native stack.
    let mut cursor = root.walk();
    let mut frames = Vec::new();
    let mut containers = Vec::<String>::new();
    if lang == Lang::CSharp {
        let mut c = root.walk();
        let namespace = root
            .named_children(&mut c)
            .find(|n| n.kind() == "file_scoped_namespace_declaration");
        if let Some(namespace) = namespace {
            containers.push(text_field(namespace, "name", src));
        }
    }
    let mut current_fn = None;
    let mut in_test = false;
    loop {
        let node = cursor.node();
        let saved = (current_fn, in_test, containers.len());
        let kind = node.kind();
        if let Some(container) = container_name(node, src, lang) {
            containers.push(container);
        }
        if lang == Lang::Rust && kind == "mod_item" {
            in_test |= text_field(node, "name", src) == "tests" || rust_attribute(node, src, true);
        }
        if lang == Lang::CSharp
            && matches!(
                kind,
                "class_declaration" | "struct_declaration" | "record_declaration"
            )
        {
            in_test |= csharp_attribute(node, src, &["TestFixture", "TestClass"]);
        }
        let definition =
            is_fn_def(kind) && (lang != Lang::CSharp || node.child_by_field_name("body").is_some());
        let name_node = if definition {
            if matches!(lang, Lang::C | Lang::Cpp) {
                node.child_by_field_name("declarator")
                    .and_then(declarator_name)
            } else {
                node.child_by_field_name("name")
            }
        } else {
            binding_name(node, lang)
        };
        if let Some(name_node) = name_node {
            let mut name = name_node.utf8_text(src).unwrap_or_default().to_string();
            if kind == "destructor_declaration" {
                name.insert(0, '~');
            }
            if lang == Lang::Go && kind == "method_declaration" {
                if let Some(receiver) = node.child_by_field_name("receiver") {
                    if let Some(param) = receiver.named_child(0) {
                        let ty = text_field(param, "type", src);
                        containers.push(type_segment(&ty));
                    }
                }
            }
            if lang == Lang::Cpp && definition {
                if let Some(parent) = name_node
                    .parent()
                    .filter(|p| p.kind() == "qualified_identifier")
                {
                    let scope = text_field(parent, "scope", src);
                    if !scope.is_empty() {
                        containers.push(scope);
                    }
                }
            }
            let qual = if containers.is_empty() {
                name.clone()
            } else {
                format!("{}::{name}", containers.join("::"))
            };
            let is_test = file_is_test
                || in_test
                || (lang == Lang::Rust
                    && kind == "function_item"
                    && rust_attribute(node, src, false))
                || (lang == Lang::CSharp
                    && csharp_attribute(
                        node,
                        src,
                        &["Fact", "Theory", "Test", "TestMethod", "TestCase"],
                    ));
            let idx = out.fns.len();
            out.fns.push(FnDef {
                name: name.clone(),
                qual,
                sig: signature(node, name_node, src, lang, definition),
                start_row: node.start_position().row,
                end_row: node.end_position().row + usize::from(node.end_position().column != 0),
                body_hash: hash_without_name(node, name_node, src),
                is_test,
            });
            current_fn = Some(idx);
            containers.push(name);
        } else if is_call(kind) {
            if let Some(caller) = current_fn {
                if let Some(name) = callee_name(node, src) {
                    out.calls.push(CallSite { caller, name });
                }
            }
        }
        frames.push(saved);
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            let (old_fn, old_test, old_len) = frames.pop().unwrap();
            current_fn = old_fn;
            in_test = old_test;
            containers.truncate(old_len);
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return out;
            }
        }
    }
}

fn text_field(node: tree_sitter::Node, field: &str, src: &[u8]) -> String {
    node.child_by_field_name(field)
        .and_then(|n| n.utf8_text(src).ok())
        .unwrap_or_default()
        .to_string()
}

fn type_segment(text: &str) -> String {
    let bare = text.trim().trim_start_matches(['*', '&']);
    bare.split(['<', '['])
        .next()
        .unwrap_or(bare)
        .rsplit("::")
        .next()
        .unwrap_or(bare)
        .trim()
        .to_string()
}

fn container_name(node: tree_sitter::Node, src: &[u8], lang: Lang) -> Option<String> {
    let kind = node.kind();
    if lang == Lang::Rust && kind == "impl_item" {
        let ty = type_segment(&text_field(node, "type", src));
        let tr = text_field(node, "trait", src);
        return Some(if tr.is_empty() {
            ty
        } else {
            format!("<{ty} as {}>", type_segment(&tr))
        });
    }
    let container = match lang {
        Lang::Rust => matches!(kind, "mod_item" | "trait_item"),
        Lang::Python => kind == "class_definition",
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => matches!(
            kind,
            "class_declaration"
                | "abstract_class_declaration"
                | "class"
                | "internal_module"
                | "module"
        ),
        Lang::CSharp => matches!(
            kind,
            "namespace_declaration"
                | "class_declaration"
                | "struct_declaration"
                | "interface_declaration"
                | "record_declaration"
                | "enum_declaration"
        ),
        Lang::Cpp => matches!(
            kind,
            "namespace_definition" | "class_specifier" | "struct_specifier" | "union_specifier"
        ),
        _ => false,
    };
    if !container {
        return None;
    }
    let mut name = text_field(node, "name", src);
    if name.is_empty() && kind == "class" {
        if let Some(parent) = node.parent() {
            name = text_field(parent, "name", src);
        }
    }
    if name.is_empty() && kind == "namespace_definition" {
        name = "(anonymous)".to_string();
    }
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn rust_attribute(node: tree_sitter::Node, src: &[u8], cfg: bool) -> bool {
    let mut previous = node.prev_named_sibling();
    while let Some(attr) = previous.filter(|n| n.kind() == "attribute_item") {
        let compact: String = attr
            .utf8_text(src)
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if cfg {
            if compact == "#[cfg(test)]" {
                return true;
            }
        } else {
            let path = compact
                .trim_start_matches("#[")
                .split(['(', ']'])
                .next()
                .unwrap_or("");
            if path.rsplit("::").next() == Some("test") {
                return true;
            }
        }
        previous = attr.prev_named_sibling();
    }
    false
}

fn csharp_attribute(node: tree_sitter::Node, src: &[u8], names: &[&str]) -> bool {
    let mut cursor = node.walk();
    for list in node
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "attribute_list")
    {
        let mut ac = list.walk();
        for attr in list
            .named_children(&mut ac)
            .filter(|n| n.kind() == "attribute")
        {
            let name = text_field(attr, "name", src);
            let simple = name.rsplit('.').next().unwrap_or(&name);
            if names.contains(&simple.strip_suffix("Attribute").unwrap_or(simple)) {
                return true;
            }
        }
    }
    false
}

fn signature(
    node: tree_sitter::Node,
    name: tree_sitter::Node,
    src: &[u8],
    lang: Lang,
    definition: bool,
) -> String {
    if definition && matches!(lang, Lang::C | Lang::Cpp) {
        let mut parent = name.parent();
        while let Some(p) = parent {
            if p.kind() == "function_declarator" {
                return std::str::from_utf8(&src[name.end_byte()..p.end_byte()])
                    .unwrap_or_default()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            if p.id() == node.id() {
                break;
            }
            parent = p.parent();
        }
    }
    let mut target = node;
    if !definition {
        if let Some(value) = node
            .child_by_field_name("value")
            .or_else(|| node.child_by_field_name("right"))
        {
            target = unparenthesized(if value.kind() == "expression_list" {
                value.named_child(0).unwrap_or(value)
            } else {
                value
            });
        } else if lang == Lang::CSharp {
            let mut c = node.walk();
            let value = node.named_children(&mut c).find(|n| {
                matches!(
                    unparenthesized(*n).kind(),
                    "lambda_expression" | "anonymous_method_expression"
                )
            });
            if let Some(value) = value {
                target = unparenthesized(value);
            }
        }
    }
    if lang == Lang::Cpp && !definition {
        if let Some(declarator) = target.child_by_field_name("declarator") {
            target = declarator;
        }
    }
    let params = target
        .child_by_field_name("parameters")
        .or_else(|| target.child_by_field_name("parameter"));
    let params = params.or_else(|| {
        let mut c = target.walk();
        let found = target.named_children(&mut c).find(|n| {
            matches!(
                n.kind(),
                "parameters"
                    | "parameter_list"
                    | "formal_parameters"
                    | "lambda_parameters"
                    | "implicit_parameter"
            )
        });
        found
    });
    params
        .and_then(|n| n.utf8_text(src).ok())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
fn simple_name(mut expr: tree_sitter::Node, src: &[u8]) -> Option<String> {
    loop {
        expr = match expr.kind() {
            "identifier"
            | "property_identifier"
            | "field_identifier"
            | "shorthand_property_identifier"
            | "type_identifier"
            | "destructor_name"
            | "operator_name" => return expr.utf8_text(src).ok().map(str::to_string),
            "parenthesized_expression" | "generic_name" => expr.named_child(0)?,
            "template_function" | "template_method" | "template_type" | "dependent_name" => {
                expr.child_by_field_name("name")?
            }
            "pointer_expression" => expr.child_by_field_name("argument")?,
            "generic_function" => expr.child_by_field_name("function")?,
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
                let mut c = expr.walk();
                let last = expr.named_children(&mut c).last();
                last?
            }
            _ => return None,
        };
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
fn declarator_name(mut node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    loop {
        node = match node.kind() {
            "identifier" | "field_identifier" | "type_identifier" | "destructor_name"
            | "operator_name" => return Some(node),
            "qualified_identifier" | "template_function" | "template_method" | "dependent_name" => {
                node.child_by_field_name("name")?
            }
            "parenthesized_declarator" | "reference_declarator" => node.named_child(0)?,
            _ => node.child_by_field_name("declarator")?,
        };
    }
}

/// FNV-1a over the complete source, excluding only the binding/name range.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_identities_and_overload_signatures() {
        for (path, src, expected) in [
            ("a.rs", "impl<T> A<T> { fn new() {} } impl B { fn new() {} } impl Display for A { fn fmt() {} } fn main() { let double = |x| x * 2; }", vec!["A::new", "B::new", "<A as Display>::fmt", "main", "main::double"]),
            ("a.go", "package a\nfunc (r *Router[T]) Handle() {}\nfunc (s Server) Handle() {}", vec!["Router::Handle", "Server::Handle"]),
            ("a.py", "class A:\n def run(self): pass\nclass B:\n def run(self): pass\n", vec!["A::run", "B::run"]),
            ("a.ts", "namespace N { class A { run() {} } } const B = class { run() {} };", vec!["N::A::run", "B::run"]),
            ("a.cs", "namespace A.B; class Worker { void Log(int x) {} void Log(string x) {} void Run() { void Local() {} } }", vec!["A.B::Worker::Log", "A.B::Worker::Log", "A.B::Worker::Run", "A.B::Worker::Run::Local"]),
            ("a.cpp", "struct A { int run() { return 0; } }; int B::run() const noexcept { return 1; }", vec!["A::run", "B::run"]),
        ] {
            let pf = parse_file(path, src).unwrap();
            assert_eq!(pf.fns.iter().map(|f| f.qual.as_str()).collect::<Vec<_>>(), expected, "{path}");
            if path == "a.cs" { assert_ne!(pf.fns[0].sig, pf.fns[1].sig); }
            if path == "a.cpp" { assert_eq!(pf.fns[1].sig, "() const noexcept"); }
        }
    }

    #[test]
    fn tsx_and_language_families() {
        let pf = parse_file("a.tsx", "const Component = () => {\n return <div>Hello</div>;\n};\nfunction go() {\n work();\n}\n").unwrap();
        assert_eq!(
            pf.fns
                .iter()
                .map(|f| (f.name.as_str(), f.start_row, f.end_row))
                .collect::<Vec<_>>(),
            [("Component", 0, 3), ("go", 3, 6)]
        );
        for path in ["a.ts", "a.tsx", "a.mts", "a.cts", "a.js", "a.cjs"] {
            assert_eq!(lang_for_path(path).unwrap().family(), Lang::TypeScript);
        }
        assert_eq!(Lang::C.family(), Lang::Cpp);
    }

    #[test]
    fn deep_expression_on_small_stack() {
        std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(|| {
                let src = format!("int run(void) {{ return 0{}; }}", "+1".repeat(20_000));
                let pf = parse_file("deep.c", &src).unwrap();
                assert_eq!(pf.fns.len(), 1);
                assert_eq!(pf.fns[0].name, "run");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn additional_test_conventions_and_attributes() {
        for path in [
            "tests/a.py",
            "test/a.py",
            "conftest.py",
            "src/a.spec.ts",
            "src/a.spec.tsx",
            "test/a.js",
        ] {
            let src = if path.ends_with(".py") {
                "def run(): pass"
            } else {
                "function run() {}"
            };
            assert!(parse_file(path, src).unwrap().fns[0].is_test, "{path}");
        }
        for path in [
            "contest/a.py",
            "src/conftest_extra.py",
            "src/a.special.ts",
            "contest/a.js",
        ] {
            let src = if path.ends_with(".py") {
                "def run(): pass"
            } else {
                "function run() {}"
            };
            assert!(!parse_file(path, src).unwrap().fns[0].is_test, "{path}");
        }
        let rust = parse_file("a.rs", "#[tokio::test]\nasync fn one() {}\n#[cfg(test)]\nmod checks { fn two() {} }\n#[contest]\nfn three() {}\n").unwrap();
        assert_eq!(
            rust.fns.iter().map(|f| f.is_test).collect::<Vec<_>>(),
            [true, true, false]
        );
        for attr in [
            "Fact",
            "Theory",
            "Test",
            "TestMethod",
            "TestCase",
            "FactAttribute",
            "Xunit.Fact",
        ] {
            let src = format!("class A {{ [{attr}] void Run() {{}} void Prod() {{}} }}");
            let pf = parse_file("a.cs", &src).unwrap();
            assert!(pf.fns[0].is_test, "{attr}");
            assert!(!pf.fns[1].is_test, "{attr}");
        }
        for attr in ["TestFixture", "TestClassAttribute"] {
            let pf =
                parse_file("a.cs", &format!("[{attr}] class A {{ void Run() {{}} }}")).unwrap();
            assert!(pf.fns[0].is_test);
        }
        assert!(
            !parse_file("a.cs", "[Contest] class A { [Latest] void Run() {} }")
                .unwrap()
                .fns[0]
                .is_test
        );
    }

    #[test]
    fn rename_hashes_across_languages() {
        for (path, source) in [
            ("a.rs", "fn old(x: u32) {}"),
            ("a.py", "def old(x): pass"),
            ("a.ts", "const old = (x) => x + 1;"),
            ("a.go", "package a\nfunc old(x int) {}"),
            ("a.rs", "fn main() { let old = |x| x + 1; }"),
            ("a.py", "old = lambda x: x + 1"),
            (
                "a.cs",
                "class A { void Run() { Func<int,int> old = x => x + 1; } }",
            ),
        ] {
            let before = parse_file(path, source).unwrap();
            let after = parse_file(path, &source.replace("old", "renamed")).unwrap();
            let index = before.fns.iter().position(|f| f.name == "old").unwrap();
            assert_eq!(
                before.fns[index].body_hash, after.fns[index].body_hash,
                "{path}"
            );
        }
    }

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
